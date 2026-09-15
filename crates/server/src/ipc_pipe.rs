//! Named-pipe IPC between Service (Session 0) and Agent (user session).
//!
//! Uses TWO separate unidirectional pipes to avoid Windows synchronous I/O
//! deadlock (only one I/O operation can be pending per handle at a time):
//! - `\\.\pipe\PhantomIPC_up_{session_id}_{generation}`   — agent → service
//! - `\\.\pipe\PhantomIPC_down_{session_id}_{generation}` — service → agent
//!
//! Pipe names include the Windows session ID and a service-owned generation.
//! The generation lets a replacement agent become ready before the active agent
//! is retired without both processes racing for the same named pipes.
//!
//! Protocol (little-endian, binary):
//! ```text
//! [u8 msg_type][u32 payload_len][payload...]
//! ```
//!
//! Message types:
//! - 0x01 EncodedFrame (agent → service): \[u8 is_keyframe\]\[u8 codec\]\[u32 width\]\[u32 height\]\[data\]
//! - 0x02 InputEvent (service → agent): bincode-serialized InputEvent
//! - 0x03 Heartbeat (bidirectional): empty payload
//! - 0x04 Shutdown (service → agent): empty payload
//! - 0x05 ForceKeyframe (service → agent): empty payload
//! - 0x06 ResolutionChange (service → agent): \[u32 width\]\[u32 height\]
//! - 0x07 PasteText (service → agent): UTF-8 text
//! - 0x08 ClipboardSync (agent → service): UTF-8 text
//! - 0x09 ViewerState (service → agent): \[u8 active\]
//! - 0x0a CursorState (agent → service): \[u8 visible\]\[i32 x\]\[i32 y\]\[u64 shape_id\]
//! - 0x0b CursorShape (agent → service): \[u64 id\]\[u32 w\]\[u32 h\]\[i32 hot_x\]\[i32 hot_y\]\[rgba\]

#[cfg(target_os = "windows")]
mod platform {
    use anyhow::{Context, Result};
    use phantom_core::encode::{EncodedFrame, VideoCodec};
    use phantom_core::input::InputEvent;
    use phantom_core::protocol::{CursorShape, CursorState};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use windows::core::{HSTRING, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, LocalFree, HANDLE, HLOCAL, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, TokenGroups, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_GROUPS,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    #[derive(Clone, Copy)]
    struct SendHandle(HANDLE);
    unsafe impl Send for SendHandle {}
    impl SendHandle {
        fn get(self) -> HANDLE {
            self.0
        }
    }

    const PIPE_BUFFER_SIZE: u32 = 4 * 1024 * 1024;
    const ERROR_IO_PENDING_CODE: u32 = 997;
    const ERROR_NO_DATA_CODE: u32 = 232;
    const ERROR_PIPE_CONNECTED_CODE: u32 = 535;
    const SE_GROUP_LOGON_ID: u32 = 0xc000_0000;

    fn is_win32_error(error: &windows::core::Error, code: u32) -> bool {
        let hresult = (0x8007_0000u32 | code) as i32;
        error.code().0 == hresult || error.code().0 == code as i32
    }

    fn duration_to_wait_ms(timeout: Option<Duration>) -> u32 {
        timeout
            .map(|d| d.as_millis().min(u32::MAX as u128) as u32)
            .unwrap_or(INFINITE)
    }

    unsafe fn create_overlapped_event() -> Result<HANDLE> {
        CreateEventW(None, true, false, None).context("CreateEventW for pipe overlapped I/O")
    }

    unsafe fn wait_overlapped(
        handle: HANDLE,
        overlapped: &mut OVERLAPPED,
        timeout: Option<Duration>,
        context: &str,
    ) -> Result<Option<u32>> {
        match WaitForSingleObject(overlapped.hEvent, duration_to_wait_ms(timeout)) {
            WAIT_OBJECT_0 => {
                let mut transferred = 0u32;
                GetOverlappedResult(handle, overlapped, &mut transferred, false)
                    .with_context(|| format!("{context}: GetOverlappedResult"))?;
                Ok(Some(transferred))
            }
            WAIT_TIMEOUT => {
                let _ = CancelIoEx(handle, Some(overlapped as *const _));
                let mut transferred = 0u32;
                let _ = GetOverlappedResult(handle, overlapped, &mut transferred, true);
                Ok(None)
            }
            WAIT_FAILED => Err(windows::core::Error::from_win32()).context(context.to_string()),
            other => anyhow::bail!("{context}: unexpected WaitForSingleObject result {other:?}"),
        }
    }

    /// Build session-isolated pipe names.
    fn pipe_names(session_id: u32, generation: u64) -> (String, String) {
        (
            format!(r"\\.\pipe\PhantomIPC_up_{session_id}_{generation}"),
            format!(r"\\.\pipe\PhantomIPC_down_{session_id}_{generation}"),
        )
    }
    const MSG_ENCODED_FRAME: u8 = 0x01;
    const MSG_INPUT: u8 = 0x02;
    const MSG_HEARTBEAT: u8 = 0x03;
    const MSG_SHUTDOWN: u8 = 0x04;
    const MSG_FORCE_KEYFRAME: u8 = 0x05;
    const MSG_RESOLUTION_CHANGE: u8 = 0x06;
    const MSG_PASTE_TEXT: u8 = 0x07;
    const MSG_CLIPBOARD_SYNC: u8 = 0x08; // agent → service (clipboard changed)
    const MSG_VIEWER_STATE: u8 = 0x09;
    const MSG_CURSOR_STATE: u8 = 0x0a;
    const MSG_CURSOR_SHAPE: u8 = 0x0b;

    // ── Low-level pipe I/O helpers ──────────────────────────────────────────

    unsafe fn pipe_write_once(handle: HANDLE, buf: &[u8]) -> Result<u32> {
        let event = create_overlapped_event()?;
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let mut written = 0u32;
        let result = WriteFile(handle, Some(buf), Some(&mut written), Some(&mut overlapped));
        let outcome = match result {
            Ok(()) => Ok(written),
            Err(e) if is_win32_error(&e, ERROR_IO_PENDING_CODE) => {
                wait_overlapped(handle, &mut overlapped, None, "pipe write")?
                    .ok_or_else(|| anyhow::anyhow!("pipe write unexpectedly timed out"))
            }
            Err(e) => Err(e).context("pipe write"),
        };
        let _ = CloseHandle(event);
        outcome
    }

    unsafe fn pipe_read_once(handle: HANDLE, buf: &mut [u8]) -> Result<u32> {
        let event = create_overlapped_event()?;
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let mut read = 0u32;
        let result = ReadFile(handle, Some(buf), Some(&mut read), Some(&mut overlapped));
        let outcome = match result {
            Ok(()) => Ok(read),
            Err(e) if is_win32_error(&e, ERROR_IO_PENDING_CODE) => {
                wait_overlapped(handle, &mut overlapped, None, "pipe read")?
                    .ok_or_else(|| anyhow::anyhow!("pipe read unexpectedly timed out"))
            }
            Err(e) => Err(e).context("pipe read"),
        };
        let _ = CloseHandle(event);
        outcome
    }

    unsafe fn pipe_write_all(handle: HANDLE, buf: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            let written = pipe_write_once(handle, &buf[offset..])?;
            if written == 0 {
                anyhow::bail!("pipe disconnected (wrote 0 bytes)");
            }
            offset += written as usize;
        }
        Ok(())
    }

    unsafe fn pipe_read_exact(handle: HANDLE, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let mut offset = 0;
        while offset < len {
            let read = pipe_read_once(handle, &mut buf[offset..])?;
            if read == 0 {
                anyhow::bail!("pipe disconnected (read 0 bytes)");
            }
            offset += read as usize;
        }
        Ok(buf)
    }

    unsafe fn send_message(handle: HANDLE, msg_type: u8, payload: &[u8]) -> Result<()> {
        let mut header = [0u8; 5];
        header[0] = msg_type;
        header[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        pipe_write_all(handle, &header)?;
        if !payload.is_empty() {
            pipe_write_all(handle, payload)?;
        }
        Ok(())
    }

    unsafe fn recv_message(handle: HANDLE) -> Result<(u8, Vec<u8>)> {
        let header = pipe_read_exact(handle, 5)?;
        let msg_type = header[0];
        let payload_len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let payload = if payload_len > 0 {
            pipe_read_exact(handle, payload_len)?
        } else {
            Vec::new()
        };
        Ok((msg_type, payload))
    }

    /// Encode an EncodedFrame into the wire format:
    /// \[u8 is_keyframe\]\[u8 codec\]\[u32 width\]\[u32 height\]\[data\]
    /// codec: 0 = H264, 1 = AV1
    fn encode_ipc_frame(frame: &EncodedFrame, width: u32, height: u32) -> Vec<u8> {
        let mut payload = Vec::with_capacity(10 + frame.data.len());
        payload.push(if frame.is_keyframe { 1 } else { 0 });
        payload.push(match frame.codec {
            VideoCodec::H264 => 0,
            VideoCodec::Av1 => 1,
        });
        payload.extend_from_slice(&width.to_le_bytes());
        payload.extend_from_slice(&height.to_le_bytes());
        payload.extend_from_slice(&frame.data);
        payload
    }

    /// Decode an EncodedFrame from the wire format.
    fn decode_ipc_frame(payload: &[u8]) -> Result<(EncodedFrame, u32, u32)> {
        if payload.len() < 10 {
            anyhow::bail!("encoded frame payload too short: {} bytes", payload.len());
        }
        let is_keyframe = payload[0] != 0;
        let codec = match payload[1] {
            0 => VideoCodec::H264,
            1 => VideoCodec::Av1,
            other => anyhow::bail!("unknown IPC codec byte: {other}"),
        };
        let width = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);
        let height = u32::from_le_bytes([payload[6], payload[7], payload[8], payload[9]]);
        let data = payload[10..].to_vec();
        Ok((
            EncodedFrame {
                codec,
                data,
                is_keyframe,
            },
            width,
            height,
        ))
    }

    fn encode_cursor_state(state: &CursorState) -> [u8; 17] {
        let mut payload = [0u8; 17];
        payload[0] = u8::from(state.visible);
        payload[1..5].copy_from_slice(&state.x.to_le_bytes());
        payload[5..9].copy_from_slice(&state.y.to_le_bytes());
        payload[9..17].copy_from_slice(&state.shape_id.to_le_bytes());
        payload
    }

    fn decode_cursor_state(payload: &[u8]) -> Result<CursorState> {
        if payload.len() < 17 {
            anyhow::bail!("cursor state payload too short: {} bytes", payload.len());
        }
        Ok(CursorState {
            visible: payload[0] != 0,
            x: i32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]),
            y: i32::from_le_bytes([payload[5], payload[6], payload[7], payload[8]]),
            shape_id: u64::from_le_bytes([
                payload[9],
                payload[10],
                payload[11],
                payload[12],
                payload[13],
                payload[14],
                payload[15],
                payload[16],
            ]),
        })
    }

    fn encode_cursor_shape(shape: &CursorShape) -> Vec<u8> {
        let mut payload = Vec::with_capacity(24 + shape.rgba.len());
        payload.extend_from_slice(&shape.shape_id.to_le_bytes());
        payload.extend_from_slice(&shape.width.to_le_bytes());
        payload.extend_from_slice(&shape.height.to_le_bytes());
        payload.extend_from_slice(&shape.hotspot_x.to_le_bytes());
        payload.extend_from_slice(&shape.hotspot_y.to_le_bytes());
        payload.extend_from_slice(&shape.rgba);
        payload
    }

    fn decode_cursor_shape(payload: &[u8]) -> Result<CursorShape> {
        if payload.len() < 24 {
            anyhow::bail!("cursor shape payload too short: {} bytes", payload.len());
        }
        let shape_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
        let width = u32::from_le_bytes(payload[8..12].try_into().unwrap());
        let height = u32::from_le_bytes(payload[12..16].try_into().unwrap());
        let hotspot_x = i32::from_le_bytes(payload[16..20].try_into().unwrap());
        let hotspot_y = i32::from_le_bytes(payload[20..24].try_into().unwrap());
        let expected = width
            .checked_mul(height)
            .and_then(|px| px.checked_mul(4))
            .map(|n| n as usize)
            .ok_or_else(|| anyhow::anyhow!("cursor shape dimensions overflow: {width}x{height}"))?;
        if payload.len() - 24 != expected {
            anyhow::bail!(
                "cursor shape rgba size mismatch: got {}, expected {} for {}x{}",
                payload.len() - 24,
                expected,
                width,
                height
            );
        }
        Ok(CursorShape {
            shape_id,
            width,
            height,
            hotspot_x,
            hotspot_y,
            rgba: payload[24..].to_vec(),
        })
    }

    fn sid_to_string(sid: windows::Win32::Security::PSID) -> Result<String> {
        unsafe {
            let mut value = PWSTR::null();
            ConvertSidToStringSidW(sid, &mut value).context("ConvertSidToStringSidW")?;
            let result = value.to_string().context("decode logon SID");
            let _ = LocalFree(HLOCAL(value.0.cast()));
            result
        }
    }

    /// Return the per-logon SID from the active session token. A user SID is
    /// shared across that user's sessions; the logon SID is unique to this
    /// interactive logon and therefore preserves the session isolation encoded
    /// in the pipe name.
    fn session_logon_sid(session_id: u32) -> Result<Option<String>> {
        unsafe {
            let mut token = HANDLE::default();
            if let Err(error) = WTSQueryUserToken(session_id, &mut token) {
                tracing::debug!(session_id, %error, "IPC: no user token available for pipe ACL");
                return Ok(None);
            }

            let result = (|| {
                let mut required = 0u32;
                let _ = GetTokenInformation(token, TokenGroups, None, 0, &mut required);
                if required == 0 {
                    anyhow::bail!("GetTokenInformation(TokenGroups) returned no size");
                }

                // Vec<usize> gives TOKEN_GROUPS pointer alignment while still
                // allowing the Win32 API to report its required byte length.
                let words = (required as usize).div_ceil(std::mem::size_of::<usize>());
                let mut buffer = vec![0usize; words];
                GetTokenInformation(
                    token,
                    TokenGroups,
                    Some(buffer.as_mut_ptr().cast()),
                    required,
                    &mut required,
                )
                .context("GetTokenInformation(TokenGroups)")?;

                let groups = &*(buffer.as_ptr() as *const TOKEN_GROUPS);
                let entries =
                    std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize);
                for entry in entries {
                    if entry.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID {
                        return sid_to_string(entry.Sid).map(Some);
                    }
                }
                Ok(None)
            })();
            let _ = CloseHandle(token);
            result
        }
    }

    fn pipe_security_sddl(logon_sid: Option<&str>) -> String {
        match logon_sid {
            Some(sid) => format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})"),
            None => "D:P(A;;GA;;;SY)(A;;GA;;;BA)".to_string(),
        }
    }

    struct PipeSecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for PipeSecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = LocalFree(HLOCAL(self.0 .0));
                }
            }
        }
    }

    fn create_pipe_security_descriptor(session_id: u32) -> Result<PipeSecurityDescriptor> {
        let logon_sid = session_logon_sid(session_id)?;
        let sddl = pipe_security_sddl(logon_sid.as_deref());
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(&sddl),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .context("build IPC pipe security descriptor")?;
        }
        tracing::debug!(
            session_id,
            user_access = logon_sid.is_some(),
            "IPC: built session pipe ACL"
        );
        Ok(PipeSecurityDescriptor(descriptor))
    }

    /// Helper: create a named pipe server-side handle.
    fn create_pipe(name: &str, session_id: u32) -> Result<HANDLE> {
        unsafe {
            let descriptor = create_pipe_security_descriptor(session_id)?;
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0 .0,
                bInheritHandle: false.into(),
            };
            let h = CreateNamedPipeW(
                &HSTRING::from(name),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                0,
                Some(&attributes),
            );
            if h.is_invalid() {
                anyhow::bail!(
                    "CreateNamedPipe({name}) failed: {}",
                    windows::core::Error::from_win32()
                );
            }
            Ok(h)
        }
    }

    fn wait_connect(handle: HANDLE, name: &str, timeout: Duration) -> Result<bool> {
        let event = unsafe { create_overlapped_event()? };
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let result = unsafe { ConnectNamedPipe(handle, Some(&mut overlapped)) };
        let outcome = match result {
            Ok(()) => Ok(true),
            Err(e) if is_win32_error(&e, ERROR_PIPE_CONNECTED_CODE) => Ok(true),
            Err(e) if is_win32_error(&e, ERROR_NO_DATA_CODE) => Ok(false),
            Err(e) if is_win32_error(&e, ERROR_IO_PENDING_CODE) => {
                let connected = unsafe {
                    wait_overlapped(
                        handle,
                        &mut overlapped,
                        Some(timeout),
                        &format!("ConnectNamedPipe({name})"),
                    )?
                }
                .is_some();
                if !connected {
                    tracing::warn!("IPC: {name} connection timed out");
                }
                Ok(connected)
            }
            Err(e) => Err(e).context(format!("ConnectNamedPipe({name})")),
        };
        let _ = unsafe { CloseHandle(event) };
        outcome
    }

    fn open_pipe(name: &str, max_attempts: u32) -> Result<HANDLE> {
        let mut last_err = None;
        for attempt in 0..max_attempts {
            match unsafe {
                CreateFileW(
                    &HSTRING::from(name),
                    (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
            } {
                Ok(h) => {
                    tracing::info!("IPC: connected to {name} on attempt {}", attempt + 1);
                    return Ok(h);
                }
                Err(e) => {
                    if attempt < max_attempts - 1 {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap()).context(format!("open {name} after {max_attempts} attempts"))
    }

    // ── IPC Server (Service side) ───────────────────────────────────────────

    /// Received encoded frame with resolution info.
    #[derive(Clone)]
    pub struct IpcEncodedFrame {
        pub encoded: EncodedFrame,
        pub width: u32,
        pub height: u32,
    }

    pub struct IpcServer {
        up_handle: HANDLE,
        down_handle: HANDLE,
        connected: bool,
        frame_rx: Option<mpsc::Receiver<IpcEncodedFrame>>,
        last_keyframe: Arc<std::sync::Mutex<Option<IpcEncodedFrame>>>,
        clipboard_rx: Option<mpsc::Receiver<String>>,
        cursor_rx: Option<mpsc::Receiver<CursorState>>,
        cursor_shape_rx: Option<mpsc::Receiver<CursorShape>>,
        input_tx: Option<mpsc::Sender<InputEvent>>,
        shutdown: Arc<AtomicBool>,
        /// Flag set by request_keyframe(), cleared by the write thread after sending.
        keyframe_requested: Arc<AtomicBool>,
        /// Flag set by send_shutdown(), cleared by the write thread after sending.
        shutdown_requested: Arc<AtomicBool>,
        /// Pending resolution change (width, height). Write thread picks it up.
        resolution_change: Arc<std::sync::Mutex<Option<(u32, u32)>>>,
        /// Pending paste text. Write thread picks it up.
        paste_text: Arc<std::sync::Mutex<Option<String>>>,
        /// Pending viewer active/idle transition. Write thread picks it up.
        viewer_state: Arc<std::sync::Mutex<Option<bool>>>,
        viewer_count: Arc<AtomicUsize>,
        _read_thread: Option<std::thread::JoinHandle<()>>,
        _write_thread: Option<std::thread::JoinHandle<()>>,
    }

    unsafe impl Send for IpcServer {}

    impl IpcServer {
        pub fn new(session_id: u32, generation: u64) -> Result<Self> {
            let (pipe_up, pipe_down) = pipe_names(session_id, generation);
            let up_handle = create_pipe(&pipe_up, session_id)?;
            let down_handle = create_pipe(&pipe_down, session_id)?;

            Ok(Self {
                up_handle,
                down_handle,
                connected: false,
                frame_rx: None,
                last_keyframe: Arc::new(std::sync::Mutex::new(None)),
                clipboard_rx: None,
                cursor_rx: None,
                cursor_shape_rx: None,
                input_tx: None,
                shutdown: Arc::new(AtomicBool::new(false)),
                keyframe_requested: Arc::new(AtomicBool::new(false)),
                shutdown_requested: Arc::new(AtomicBool::new(false)),
                resolution_change: Arc::new(std::sync::Mutex::new(None)),
                paste_text: Arc::new(std::sync::Mutex::new(None)),
                viewer_state: Arc::new(std::sync::Mutex::new(None)),
                viewer_count: Arc::new(AtomicUsize::new(0)),
                _read_thread: None,
                _write_thread: None,
            })
        }

        pub fn wait_for_connection(&mut self, timeout: Duration) -> Result<bool> {
            tracing::info!(
                "IPC: waiting for agent on both pipes (timeout {:?})",
                timeout
            );
            let start = Instant::now();
            if !wait_connect(self.up_handle, "up", timeout)? {
                return Ok(false);
            }
            let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
                tracing::warn!("IPC: down connection timed out before wait");
                return Ok(false);
            };
            if !wait_connect(self.down_handle, "down", remaining)? {
                return Ok(false);
            }
            self.connected = true;
            self.start_io()?;
            tracing::info!("IPC: agent connected on both pipes");
            Ok(true)
        }

        fn start_io(&mut self) -> Result<()> {
            // Preserve the encoded stream exactly. Dropping an arbitrary H.264
            // delta frame corrupts every dependent frame until the next IDR.
            // The bounded channel intentionally backpressures the local pipe.
            let (frame_tx, frame_rx) = mpsc::sync_channel(30);
            let (clipboard_tx, clipboard_rx) = mpsc::sync_channel::<String>(4);
            let (cursor_tx, cursor_rx) = mpsc::sync_channel::<CursorState>(16);
            let (cursor_shape_tx, cursor_shape_rx) = mpsc::sync_channel::<CursorShape>(8);
            let (input_tx, input_rx) = mpsc::channel::<InputEvent>();
            self.frame_rx = Some(frame_rx);
            self.clipboard_rx = Some(clipboard_rx);
            self.cursor_rx = Some(cursor_rx);
            self.cursor_shape_rx = Some(cursor_shape_rx);
            self.input_tx = Some(input_tx);
            *self.last_keyframe.lock().unwrap_or_else(|e| e.into_inner()) = None;

            // Read thread: reads encoded H.264 frames from upstream pipe
            let up = SendHandle(self.up_handle);
            let shutdown = Arc::clone(&self.shutdown);
            let last_keyframe = Arc::clone(&self.last_keyframe);
            let read_thread =
                std::thread::Builder::new()
                    .name("ipc-read".into())
                    .spawn(move || {
                        let handle = up.get();
                        while !shutdown.load(Ordering::Relaxed) {
                            match unsafe { recv_message(handle) } {
                                Ok((MSG_ENCODED_FRAME, payload)) => {
                                    match decode_ipc_frame(&payload) {
                                        Ok((encoded, w, h)) => {
                                            let frame = IpcEncodedFrame {
                                                encoded,
                                                width: w,
                                                height: h,
                                            };
                                            if frame.encoded.is_keyframe {
                                                *last_keyframe
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner()) =
                                                    Some(frame.clone());
                                            }
                                            if frame_tx.send(frame).is_err() {
                                                break;
                                            }
                                        }
                                        Err(e) => tracing::warn!("IPC: bad encoded frame: {e}"),
                                    }
                                }
                                Ok((MSG_HEARTBEAT, _)) => {}
                                Ok((MSG_CLIPBOARD_SYNC, payload)) => {
                                    if let Ok(text) = String::from_utf8(payload) {
                                        let _ = clipboard_tx.try_send(text);
                                    }
                                }
                                Ok((MSG_CURSOR_STATE, payload)) => {
                                    match decode_cursor_state(&payload) {
                                        Ok(state) => {
                                            let _ = cursor_tx.try_send(state);
                                        }
                                        Err(e) => tracing::warn!("IPC: bad cursor state: {e}"),
                                    }
                                }
                                Ok((MSG_CURSOR_SHAPE, payload)) => {
                                    match decode_cursor_shape(&payload) {
                                        Ok(shape) => {
                                            let _ = cursor_shape_tx.try_send(shape);
                                        }
                                        Err(e) => tracing::warn!("IPC: bad cursor shape: {e}"),
                                    }
                                }
                                Ok((t, _)) => tracing::debug!("IPC up: unexpected 0x{t:02x}"),
                                Err(e) => {
                                    if !shutdown.load(Ordering::Relaxed) {
                                        tracing::warn!("IPC read error: {e}");
                                    }
                                    break;
                                }
                            }
                        }
                    })?;

            // Write thread: sole owner of writes to the downstream pipe.
            // Checks AtomicBool flags for keyframe/shutdown requests to avoid
            // concurrent WriteFile calls from multiple threads.
            let down = SendHandle(self.down_handle);
            let shutdown2 = Arc::clone(&self.shutdown);
            let kf_flag = Arc::clone(&self.keyframe_requested);
            let shutdown_flag = Arc::clone(&self.shutdown_requested);
            let res_change = Arc::clone(&self.resolution_change);
            let paste_text = Arc::clone(&self.paste_text);
            let viewer_state = Arc::clone(&self.viewer_state);
            let write_thread =
                std::thread::Builder::new()
                    .name("ipc-write".into())
                    .spawn(move || {
                        let handle = down.get();
                        let mut heartbeat_elapsed = Instant::now();
                        while !shutdown2.load(Ordering::Relaxed) {
                            // Check shutdown request flag (set by send_shutdown)
                            if shutdown_flag.swap(false, Ordering::SeqCst) {
                                let _ = unsafe { send_message(handle, MSG_SHUTDOWN, &[]) };
                                break;
                            }

                            if let Some(active) = viewer_state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .take()
                            {
                                let payload = [u8::from(active)];
                                if let Err(e) =
                                    unsafe { send_message(handle, MSG_VIEWER_STATE, &payload) }
                                {
                                    if !shutdown2.load(Ordering::Relaxed) {
                                        tracing::warn!("IPC viewer-state write error: {e}");
                                    }
                                    break;
                                }
                            }

                            // Send resize before keyframe when both are pending.
                            // A new web client sends a viewport hint before Hello;
                            // if FORCE_KEYFRAME reaches the agent first, it emits
                            // a keyframe at the old mode and service startup can
                            // lock onto a stale/blurred first frame.
                            if let Some((w, h)) =
                                res_change.lock().unwrap_or_else(|e| e.into_inner()).take()
                            {
                                let payload = [
                                    w.to_le_bytes()[0],
                                    w.to_le_bytes()[1],
                                    w.to_le_bytes()[2],
                                    w.to_le_bytes()[3],
                                    h.to_le_bytes()[0],
                                    h.to_le_bytes()[1],
                                    h.to_le_bytes()[2],
                                    h.to_le_bytes()[3],
                                ];
                                if let Err(e) =
                                    unsafe { send_message(handle, MSG_RESOLUTION_CHANGE, &payload) }
                                {
                                    if !shutdown2.load(Ordering::Relaxed) {
                                        tracing::warn!("IPC resolution change write error: {e}");
                                    }
                                    break;
                                }
                            }

                            // Check keyframe request flag (set by request_keyframe)
                            if kf_flag.swap(false, Ordering::SeqCst) {
                                tracing::info!("IPC write thread: sending FORCE_KEYFRAME");
                                if let Err(e) =
                                    unsafe { send_message(handle, MSG_FORCE_KEYFRAME, &[]) }
                                {
                                    if !shutdown2.load(Ordering::Relaxed) {
                                        tracing::warn!("IPC keyframe write error: {e}");
                                    }
                                    break;
                                }
                            }

                            // Check paste text request
                            if let Some(text) =
                                paste_text.lock().unwrap_or_else(|e| e.into_inner()).take()
                            {
                                let payload = text.into_bytes();
                                if let Err(e) =
                                    unsafe { send_message(handle, MSG_PASTE_TEXT, &payload) }
                                {
                                    if !shutdown2.load(Ordering::Relaxed) {
                                        tracing::warn!("IPC paste write error: {e}");
                                    }
                                    break;
                                }
                            }

                            // Drain input events (200ms timeout for responsive flag checking)
                            match input_rx.recv_timeout(Duration::from_millis(200)) {
                                Ok(event) => {
                                    let payload = match bincode::serialize(&event) {
                                        Ok(p) => p,
                                        Err(e) => {
                                            tracing::warn!("IPC serialize: {e}");
                                            continue;
                                        }
                                    };
                                    if let Err(e) =
                                        unsafe { send_message(handle, MSG_INPUT, &payload) }
                                    {
                                        if !shutdown2.load(Ordering::Relaxed) {
                                            tracing::warn!("IPC write error: {e}");
                                        }
                                        break;
                                    }
                                    heartbeat_elapsed = Instant::now();
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {
                                    // Send heartbeat every 5s of inactivity
                                    if heartbeat_elapsed.elapsed() >= Duration::from_secs(5) {
                                        if let Err(e) =
                                            unsafe { send_message(handle, MSG_HEARTBEAT, &[]) }
                                        {
                                            if !shutdown2.load(Ordering::Relaxed) {
                                                tracing::warn!("IPC heartbeat error: {e}");
                                            }
                                            break;
                                        }
                                        heartbeat_elapsed = Instant::now();
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Disconnected) => {
                                    // input_tx dropped (session ended) — don't exit.
                                    // Keep running to send heartbeats and keyframe requests.
                                    // Agent must stay alive across session boundaries.
                                    std::thread::sleep(Duration::from_millis(200));
                                }
                            }
                        }
                    })?;

            self._read_thread = Some(read_thread);
            self._write_thread = Some(write_thread);
            Ok(())
        }

        /// Receive clipboard text from agent (if any).
        pub fn recv_clipboard(&self) -> Option<String> {
            self.clipboard_rx.as_ref().and_then(|rx| rx.try_recv().ok())
        }

        /// Receive the latest cursor state from agent, dropping older queued states.
        pub fn recv_cursor_state(&self) -> Option<CursorState> {
            let mut latest = None;
            if let Some(ref rx) = self.cursor_rx {
                while let Ok(state) = rx.try_recv() {
                    latest = Some(state);
                }
            }
            latest
        }

        /// Receive queued cursor shape bitmaps from the agent.
        pub fn recv_cursor_shapes(&self) -> Vec<CursorShape> {
            let mut shapes = Vec::new();
            if let Some(ref rx) = self.cursor_shape_rx {
                while let Ok(shape) = rx.try_recv() {
                    shapes.push(shape);
                }
            }
            shapes
        }

        /// Receive all queued encoded frames from the agent.
        /// H.264 frames MUST be forwarded in order — never skip frames.
        pub fn recv_encoded_frames(&self) -> Vec<IpcEncodedFrame> {
            let mut frames = Vec::new();
            if let Some(ref rx) = self.frame_rx {
                while let Ok(frame) = rx.try_recv() {
                    frames.push(frame);
                }
            }
            frames
        }

        /// Return the latest keyframe seen from the agent, even if the normal
        /// frame queue has already been drained by a previous viewer.
        pub fn last_keyframe(&self) -> Option<IpcEncodedFrame> {
            self.last_keyframe
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        /// Send an input event to the agent for injection.
        #[allow(dead_code)]
        pub fn send_input(&self, event: InputEvent) -> Result<()> {
            if let Some(ref tx) = self.input_tx {
                tx.send(event).context("IPC input channel closed")?;
            }
            Ok(())
        }

        pub fn input_sender(&self) -> Option<mpsc::Sender<InputEvent>> {
            self.input_tx.clone()
        }

        /// Request the agent to send a keyframe.
        /// Sets a flag that the write thread picks up (avoids concurrent pipe writes).
        pub fn request_keyframe(&self) -> Result<()> {
            tracing::info!(connected = self.connected, "IPC: request_keyframe called");
            if self.connected {
                self.keyframe_requested.store(true, Ordering::SeqCst);
            }
            Ok(())
        }

        /// Get a clone of the resolution change Arc (for closures).
        pub fn resolution_change_arc(&self) -> Arc<std::sync::Mutex<Option<(u32, u32)>>> {
            Arc::clone(&self.resolution_change)
        }

        /// Request the agent to change display resolution.
        #[allow(dead_code)]
        pub fn request_resolution_change(&self, width: u32, height: u32) {
            if self.connected {
                *self
                    .resolution_change
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some((width, height));
            }
        }

        /// Get a clone of the paste text Arc (for closures).
        pub fn paste_arc(&self) -> Arc<std::sync::Mutex<Option<String>>> {
            Arc::clone(&self.paste_text)
        }

        /// Send paste text to agent for injection.
        #[allow(dead_code)]
        pub fn send_paste(&self, text: &str) {
            if self.connected {
                *self.paste_text.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.to_string());
            }
        }

        pub fn set_viewer_active(&self, active: bool) {
            if self.connected {
                *self.viewer_state.lock().unwrap_or_else(|e| e.into_inner()) = Some(active);
            }
        }

        pub fn acquire_viewer(&self) {
            if self.viewer_count.fetch_add(1, Ordering::SeqCst) == 0 {
                self.set_viewer_active(true);
            }
        }

        pub fn release_viewer(&self) {
            let prev = self
                .viewer_count
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    Some(count.saturating_sub(1))
                })
                .unwrap_or(0);
            if prev <= 1 {
                self.set_viewer_active(false);
            }
        }

        /// Request orderly shutdown of the agent.
        /// Sets a flag that the write thread picks up (avoids concurrent pipe writes).
        pub fn send_shutdown(&self) -> Result<()> {
            if self.connected {
                self.shutdown_requested.store(true, Ordering::SeqCst);
            }
            Ok(())
        }

        pub fn is_connected(&self) -> bool {
            if !self.connected {
                return false;
            }
            // Check if IO threads are still alive — a dead thread means
            // the pipe broke and this IPC is no longer usable.
            let read_dead = self._read_thread.as_ref().is_none_or(|h| h.is_finished());
            let write_dead = self._write_thread.as_ref().is_none_or(|h| h.is_finished());
            if read_dead || write_dead {
                tracing::warn!(
                    read_dead,
                    write_dead,
                    "IPC IO thread died — marking disconnected"
                );
                return false;
            }
            true
        }

        pub fn disconnect(&mut self) {
            if self.connected {
                // Ask the agent to exit before breaking the pipe. The write
                // thread checks `shutdown_requested` inside its loop; setting
                // `shutdown` first makes it exit before the shutdown message
                // can be sent.
                let _ = self.send_shutdown();
                std::thread::sleep(Duration::from_millis(250));
                self.shutdown.store(true, Ordering::SeqCst);
                unsafe {
                    let _ = CancelIoEx(self.up_handle, None);
                    let _ = CancelIoEx(self.down_handle, None);
                    let _ = DisconnectNamedPipe(self.up_handle);
                    let _ = DisconnectNamedPipe(self.down_handle);
                }
                self.connected = false;
            } else {
                self.shutdown.store(true, Ordering::SeqCst);
            }
            // Drop receivers before joining so a read thread blocked on a
            // full bounded channel observes disconnection and exits.
            self.frame_rx = None;
            *self.last_keyframe.lock().unwrap_or_else(|e| e.into_inner()) = None;
            self.clipboard_rx = None;
            self.cursor_rx = None;
            self.cursor_shape_rx = None;
            self.input_tx = None;
            if let Some(thread) = self._read_thread.take() {
                let _ = thread.join();
            }
            if let Some(thread) = self._write_thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Drop for IpcServer {
        fn drop(&mut self) {
            self.disconnect();
            unsafe {
                let _ = CloseHandle(self.up_handle);
                let _ = CloseHandle(self.down_handle);
            }
        }
    }

    // ── IPC Client (Agent side) ─────────────────────────────────────────────

    pub struct IpcClient {
        up_handle: HANDLE,
        down_handle: HANDLE,
        shutdown: Arc<AtomicBool>,
        keyframe_requested: Arc<AtomicBool>,
        viewer_active: Arc<AtomicBool>,
        resolution_requested: Arc<std::sync::Mutex<Option<(u32, u32)>>>,
        paste_requested: Arc<std::sync::Mutex<Option<String>>>,
        input_rx: Option<mpsc::Receiver<InputEvent>>,
        _read_thread: Option<std::thread::JoinHandle<()>>,
    }

    unsafe impl Send for IpcClient {}

    impl IpcClient {
        /// Connect to the service's IPC pipes.
        /// If `session_id` is provided, uses it directly. Otherwise, auto-detects
        /// from the current process's session ID via ProcessIdToSessionId.
        pub fn connect(session_id: Option<u32>, generation: Option<u64>) -> Result<Self> {
            let sid = match session_id {
                Some(id) => id,
                None => {
                    // Auto-detect session ID from current process
                    let mut sid: u32 = 0;
                    let pid = std::process::id();
                    unsafe {
                        extern "system" {
                            fn ProcessIdToSessionId(process_id: u32, session_id: *mut u32) -> i32;
                        }
                        if ProcessIdToSessionId(pid, &mut sid) == 0 {
                            anyhow::bail!("ProcessIdToSessionId failed for PID {pid}");
                        }
                    }
                    tracing::info!(session_id = sid, "Auto-detected IPC session ID");
                    sid
                }
            };
            let generation = generation.unwrap_or(0);
            let (pipe_up, pipe_down) = pipe_names(sid, generation);
            let up_handle = open_pipe(&pipe_up, 50)?;
            let down_handle = open_pipe(&pipe_down, 50)?;

            let shutdown = Arc::new(AtomicBool::new(false));
            let keyframe_requested = Arc::new(AtomicBool::new(false));
            let viewer_active = Arc::new(AtomicBool::new(false));
            let resolution_requested: Arc<std::sync::Mutex<Option<(u32, u32)>>> =
                Arc::new(std::sync::Mutex::new(None));
            let paste_requested: Arc<std::sync::Mutex<Option<String>>> =
                Arc::new(std::sync::Mutex::new(None));

            let (input_tx, input_rx) = mpsc::channel();
            let down = SendHandle(down_handle);
            let read_shutdown = Arc::clone(&shutdown);
            let read_kf = Arc::clone(&keyframe_requested);
            let read_viewer = Arc::clone(&viewer_active);
            let read_res = Arc::clone(&resolution_requested);
            let read_paste = Arc::clone(&paste_requested);

            let read_thread = std::thread::Builder::new()
                .name("ipc-agent-read".into())
                .spawn(move || {
                    let handle = down.get();
                    while !read_shutdown.load(Ordering::Relaxed) {
                        match unsafe { recv_message(handle) } {
                            Ok((MSG_INPUT, payload)) => {
                                match bincode::deserialize::<InputEvent>(&payload) {
                                    Ok(event) => {
                                        let _ = input_tx.send(event);
                                    }
                                    Err(e) => tracing::warn!("IPC: deserialize input: {e}"),
                                }
                            }
                            Ok((MSG_SHUTDOWN, _)) => {
                                tracing::info!("IPC: shutdown from service");
                                read_shutdown.store(true, Ordering::SeqCst);
                                break;
                            }
                            Ok((MSG_FORCE_KEYFRAME, _)) => {
                                read_kf.store(true, Ordering::SeqCst);
                            }
                            Ok((MSG_VIEWER_STATE, payload)) => {
                                let active = payload.first().copied().unwrap_or(0) != 0;
                                read_viewer.store(active, Ordering::SeqCst);
                            }
                            Ok((MSG_PASTE_TEXT, payload)) => {
                                if let Ok(text) = String::from_utf8(payload) {
                                    tracing::info!(len = text.len(), "IPC: paste text received");
                                    *read_paste.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some(text);
                                }
                            }
                            Ok((MSG_RESOLUTION_CHANGE, payload)) if payload.len() >= 8 => {
                                let w = u32::from_le_bytes([
                                    payload[0], payload[1], payload[2], payload[3],
                                ]);
                                let h = u32::from_le_bytes([
                                    payload[4], payload[5], payload[6], payload[7],
                                ]);
                                tracing::info!(w, h, "IPC: resolution change request");
                                *read_res.lock().unwrap_or_else(|e| e.into_inner()) = Some((w, h));
                            }
                            Ok((MSG_HEARTBEAT, _)) => {}
                            Ok((t, _)) => tracing::debug!("IPC down: unexpected 0x{t:02x}"),
                            Err(e) => {
                                if !read_shutdown.load(Ordering::Relaxed) {
                                    tracing::warn!("IPC agent read error: {e}");
                                }
                                read_shutdown.store(true, Ordering::SeqCst);
                                break;
                            }
                        }
                    }
                })?;

            tracing::info!("IPC: connected to service (two pipes)");
            Ok(Self {
                up_handle,
                down_handle,
                shutdown,
                keyframe_requested,
                viewer_active,
                resolution_requested,
                paste_requested,
                input_rx: Some(input_rx),
                _read_thread: Some(read_thread),
            })
        }

        /// Send an encoded H.264 frame to the service via upstream pipe.
        pub fn send_encoded_frame(
            &self,
            frame: &EncodedFrame,
            width: u32,
            height: u32,
        ) -> Result<()> {
            if frame.data.is_empty() {
                return Ok(());
            }
            let payload = encode_ipc_frame(frame, width, height);
            unsafe { send_message(self.up_handle, MSG_ENCODED_FRAME, &payload) }
        }

        /// Send clipboard text to service (for forwarding to client).
        pub fn send_clipboard(&self, text: &str) -> Result<()> {
            unsafe { send_message(self.up_handle, MSG_CLIPBOARD_SYNC, text.as_bytes()) }
        }

        /// Send cursor state to service for forwarding to the viewer.
        pub fn send_cursor_state(&self, state: &CursorState) -> Result<()> {
            let payload = encode_cursor_state(state);
            unsafe { send_message(self.up_handle, MSG_CURSOR_STATE, &payload) }
        }

        /// Send cursor bitmap shape to service for forwarding to the viewer.
        pub fn send_cursor_shape(&self, shape: &CursorShape) -> Result<()> {
            let payload = encode_cursor_shape(shape);
            unsafe { send_message(self.up_handle, MSG_CURSOR_SHAPE, &payload) }
        }

        /// Check and clear the keyframe request flag.
        pub fn take_keyframe_request(&self) -> bool {
            self.keyframe_requested.swap(false, Ordering::SeqCst)
        }

        pub fn viewer_active(&self) -> bool {
            self.viewer_active.load(Ordering::Relaxed)
        }

        /// Take pending paste text (if any).
        pub fn take_paste_request(&self) -> Option<String> {
            self.paste_requested
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
        }

        /// Take pending resolution change request (if any).
        pub fn take_resolution_request(&self) -> Option<(u32, u32)> {
            self.resolution_requested
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
        }

        pub fn recv_inputs(&self) -> Vec<InputEvent> {
            let mut events = Vec::new();
            if let Some(ref rx) = self.input_rx {
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
            }
            events
        }

        pub fn should_shutdown(&self) -> bool {
            self.shutdown.load(Ordering::Relaxed)
        }
    }

    impl Drop for IpcClient {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            unsafe {
                let _ = CancelIoEx(self.down_handle, None);
            }
            if let Some(thread) = self._read_thread.take() {
                let _ = thread.join();
            }
            unsafe {
                let _ = CloseHandle(self.up_handle);
                let _ = CloseHandle(self.down_handle);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{decode_cursor_shape, encode_cursor_shape, pipe_security_sddl};
        use phantom_core::protocol::CursorShape;

        #[test]
        fn pipe_acl_without_user_is_system_only() {
            assert_eq!(pipe_security_sddl(None), "D:P(A;;GA;;;SY)(A;;GA;;;BA)");
        }

        #[test]
        fn pipe_acl_grants_only_the_session_logon_sid() {
            let sddl = pipe_security_sddl(Some("S-1-5-5-1-2"));
            assert_eq!(sddl, "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;S-1-5-5-1-2)");
            assert!(!sddl.contains(";;;WD"));
            assert!(!sddl.contains(";;;AU"));
            assert!(!sddl.contains(";;;IU"));
        }

        #[test]
        fn cursor_shape_round_trips_without_duplicate_pixels() {
            let shape = CursorShape {
                shape_id: 42,
                width: 2,
                height: 1,
                hotspot_x: 1,
                hotspot_y: 0,
                rgba: vec![0, 1, 2, 3, 4, 5, 6, 7],
            };
            let encoded = encode_cursor_shape(&shape);
            assert_eq!(encoded.len(), 24 + shape.rgba.len());

            let decoded = decode_cursor_shape(&encoded).expect("cursor shape should decode");
            assert_eq!(decoded, shape);
        }
    }
}

// ── Non-Windows stubs ───────────────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
mod platform {
    use anyhow::Result;
    use phantom_core::encode::EncodedFrame;
    use phantom_core::input::InputEvent;
    use phantom_core::protocol::{CursorShape, CursorState};
    use std::time::Duration;

    pub struct IpcEncodedFrame {
        pub encoded: EncodedFrame,
        pub width: u32,
        pub height: u32,
    }

    pub struct IpcServer;
    impl IpcServer {
        pub fn new(_session_id: u32, _generation: u64) -> Result<Self> {
            anyhow::bail!("IPC pipes are only supported on Windows")
        }
        pub fn wait_for_connection(&mut self, _timeout: Duration) -> Result<bool> {
            Ok(false)
        }
        pub fn recv_encoded_frames(&self) -> Vec<IpcEncodedFrame> {
            Vec::new()
        }
        pub fn recv_cursor_state(&self) -> Option<CursorState> {
            None
        }
        pub fn recv_cursor_shapes(&self) -> Vec<CursorShape> {
            Vec::new()
        }
        pub fn send_input(&self, _event: InputEvent) -> Result<()> {
            Ok(())
        }
        pub fn input_sender(&self) -> Option<std::sync::mpsc::Sender<InputEvent>> {
            None
        }
        pub fn request_keyframe(&self) -> Result<()> {
            Ok(())
        }
        pub fn send_shutdown(&self) -> Result<()> {
            Ok(())
        }
        pub fn is_connected(&self) -> bool {
            false
        }
        pub fn disconnect(&mut self) {}
    }

    pub struct IpcClient;
    impl IpcClient {
        pub fn connect(_session_id: Option<u32>, _generation: Option<u64>) -> Result<Self> {
            anyhow::bail!("IPC pipes are only supported on Windows")
        }
        pub fn send_encoded_frame(&self, _frame: &EncodedFrame, _w: u32, _h: u32) -> Result<()> {
            Ok(())
        }
        pub fn send_cursor_state(&self, _state: &CursorState) -> Result<()> {
            Ok(())
        }
        pub fn send_cursor_shape(&self, _shape: &CursorShape) -> Result<()> {
            Ok(())
        }
        pub fn take_keyframe_request(&self) -> bool {
            false
        }
        pub fn recv_inputs(&self) -> Vec<InputEvent> {
            Vec::new()
        }
        pub fn should_shutdown(&self) -> bool {
            false
        }
    }
}

#[allow(unused_imports)]
pub use platform::{IpcClient, IpcEncodedFrame, IpcServer};
