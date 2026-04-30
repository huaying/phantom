//! Named-pipe IPC between Service (Session 0) and Agent (user session).
//!
//! Uses TWO separate unidirectional pipes to avoid Windows synchronous I/O
//! deadlock (only one I/O operation can be pending per handle at a time):
//! - `\\.\pipe\PhantomIPC_up_{session_id}`   — agent → service (encoded frames, heartbeat)
//! - `\\.\pipe\PhantomIPC_down_{session_id}` — service → agent (input, heartbeat, shutdown, keyframe-request)
//!
//! Pipe names include the Windows session ID for isolation between multiple
//! concurrent user sessions.
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
//! - 0x09 ViewerState (service → agent): \[u8 active\]

#[cfg(target_os = "windows")]
mod platform {
    use anyhow::{Context, Result};
    use phantom_core::encode::{EncodedFrame, VideoCodec};
    use phantom_core::input::InputEvent;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    #[derive(Clone, Copy)]
    struct SendHandle(HANDLE);
    unsafe impl Send for SendHandle {}
    impl SendHandle {
        fn get(self) -> HANDLE {
            self.0
        }
    }

    const PIPE_BUFFER_SIZE: u32 = 4 * 1024 * 1024;

    /// Build session-isolated pipe names.
    fn pipe_names(session_id: u32) -> (String, String) {
        (
            format!(r"\\.\pipe\PhantomIPC_up_{session_id}"),
            format!(r"\\.\pipe\PhantomIPC_down_{session_id}"),
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

    // ── Low-level pipe I/O helpers ──────────────────────────────────────────

    unsafe fn pipe_write_all(handle: HANDLE, buf: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            let mut written = 0u32;
            WriteFile(handle, Some(&buf[offset..]), Some(&mut written), None)
                .context("pipe write")?;
            offset += written as usize;
        }
        Ok(())
    }

    unsafe fn pipe_read_exact(handle: HANDLE, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let mut offset = 0;
        while offset < len {
            let mut read = 0u32;
            ReadFile(handle, Some(&mut buf[offset..]), Some(&mut read), None)
                .context("pipe read")?;
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

    /// Helper: create a named pipe server-side handle.
    fn create_pipe(name: &str) -> Result<HANDLE> {
        unsafe {
            let h = CreateNamedPipeW(
                &HSTRING::from(name),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                0,
                None,
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
        let start = Instant::now();
        // ConnectNamedPipe is blocking (no OVERLAPPED), so every branch below
        // returns — the `loop {}` is kept as a style bookmark for a future
        // async version that would actually retry.
        #[allow(clippy::never_loop)]
        loop {
            if start.elapsed() > timeout {
                tracing::warn!("IPC: {name} connection timed out");
                return Ok(false);
            }
            let result = unsafe { ConnectNamedPipe(handle, None) };
            match result {
                Ok(()) => return Ok(true),
                Err(e) => {
                    let code = e.code().0;
                    if code == 0x80070217u32 as i32 || code == 535 {
                        return Ok(true);
                    }
                    if code == 0x800700E8u32 as i32 || code == 232 {
                        return Ok(false);
                    }
                    return Err(e).context(format!("ConnectNamedPipe({name})"));
                }
            }
        }
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
        pub fn new(session_id: u32) -> Result<Self> {
            let (pipe_up, pipe_down) = pipe_names(session_id);
            let up_handle = create_pipe(&pipe_up)?;
            let down_handle = create_pipe(&pipe_down)?;

            Ok(Self {
                up_handle,
                down_handle,
                connected: false,
                frame_rx: None,
                last_keyframe: Arc::new(std::sync::Mutex::new(None)),
                clipboard_rx: None,
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
            if !wait_connect(self.up_handle, "up", timeout)? {
                return Ok(false);
            }
            if !wait_connect(self.down_handle, "down", timeout)? {
                return Ok(false);
            }
            self.connected = true;
            self.start_io()?;
            tracing::info!("IPC: agent connected on both pipes");
            Ok(true)
        }

        fn start_io(&mut self) -> Result<()> {
            // Bounded channel — drops old frames when no session is draining.
            let (frame_tx, frame_rx) = mpsc::sync_channel(30);
            let (clipboard_tx, clipboard_rx) = mpsc::sync_channel::<String>(4);
            let (input_tx, input_rx) = mpsc::channel::<InputEvent>();
            self.frame_rx = Some(frame_rx);
            self.clipboard_rx = Some(clipboard_rx);
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
                                            // try_send: drop frame if buffer full (backpressure).
                                            let _ = frame_tx.try_send(frame);
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
            self.shutdown.store(true, Ordering::SeqCst);
            if self.connected {
                let _ = self.send_shutdown();
                unsafe {
                    let _ = DisconnectNamedPipe(self.up_handle);
                    let _ = DisconnectNamedPipe(self.down_handle);
                }
                self.connected = false;
            }
            self.frame_rx = None;
            *self.last_keyframe.lock().unwrap_or_else(|e| e.into_inner()) = None;
            self.clipboard_rx = None;
            self.input_tx = None;
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
        pub fn connect(session_id: Option<u32>) -> Result<Self> {
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
            let (pipe_up, pipe_down) = pipe_names(sid);
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
            let payload = encode_ipc_frame(frame, width, height);
            unsafe { send_message(self.up_handle, MSG_ENCODED_FRAME, &payload) }
        }

        /// Send clipboard text to service (for forwarding to client).
        pub fn send_clipboard(&self, text: &str) -> Result<()> {
            unsafe { send_message(self.up_handle, MSG_CLIPBOARD_SYNC, text.as_bytes()) }
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
                let _ = CloseHandle(self.up_handle);
                let _ = CloseHandle(self.down_handle);
            }
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
    use std::time::Duration;

    pub struct IpcEncodedFrame {
        pub encoded: EncodedFrame,
        pub width: u32,
        pub height: u32,
    }

    pub struct IpcServer;
    impl IpcServer {
        pub fn new(_session_id: u32) -> Result<Self> {
            anyhow::bail!("IPC pipes are only supported on Windows")
        }
        pub fn wait_for_connection(&mut self, _timeout: Duration) -> Result<bool> {
            Ok(false)
        }
        pub fn recv_encoded_frames(&self) -> Vec<IpcEncodedFrame> {
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
        pub fn connect(_session_id: Option<u32>) -> Result<Self> {
            anyhow::bail!("IPC pipes are only supported on Windows")
        }
        pub fn send_encoded_frame(&self, _frame: &EncodedFrame, _w: u32, _h: u32) -> Result<()> {
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
