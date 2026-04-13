//! Named-pipe IPC between Service (Session 0) and Agent (user session).
//!
//! Protocol (little-endian, binary):
//! ```text
//! [u8 msg_type][u32 payload_len][payload...]
//! ```
//!
//! Message types:
//! - 0x01 Frame (agent → service): [u32 width][u32 height][zstd-compressed BGRA data]
//! - 0x02 InputEvent (service → agent): bincode-serialized InputEvent
//! - 0x03 Heartbeat (bidirectional): empty payload
//! - 0x04 Shutdown (service → agent): empty payload
//!
//! Frames are zstd-compressed before sending (~8MB raw → ~200KB-2MB compressed)
//! to keep pipe throughput manageable at 30 FPS.

#[cfg(target_os = "windows")]
mod platform {
    use anyhow::{Context, Result};
    use phantom_core::frame::{Frame, PixelFormat};
    use phantom_core::input::InputEvent;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe,
        PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    /// Wrapper to allow HANDLE to be sent between threads.
    /// HANDLE is a kernel object handle (pointer-sized integer) — safe to use from any thread.
    #[derive(Clone, Copy)]
    struct SendHandle(HANDLE);
    unsafe impl Send for SendHandle {}

    impl SendHandle {
        fn get(self) -> HANDLE {
            self.0
        }
    }

    const PIPE_NAME: &str = r"\\.\pipe\PhantomIPC";
    const PIPE_BUFFER_SIZE: u32 = 4 * 1024 * 1024; // 4MB buffer
    const MSG_FRAME: u8 = 0x01;
    const MSG_INPUT: u8 = 0x02;
    const MSG_HEARTBEAT: u8 = 0x03;
    const MSG_SHUTDOWN: u8 = 0x04;
    const ZSTD_LEVEL: i32 = 1; // Fast compression for real-time frames

    // ── Low-level pipe I/O helpers ──────────────────────────────────────────

    /// Write exactly `buf` bytes to a pipe handle.
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

    /// Read exactly `len` bytes from a pipe handle.
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

    /// Send a message over the pipe.
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

    /// Receive a message from the pipe. Returns (msg_type, payload).
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

    /// Encode a Frame into the wire format: [u32 width][u32 height][zstd(bgra_data)]
    fn encode_frame(frame: &Frame) -> Result<Vec<u8>> {
        let compressed =
            zstd::encode_all(frame.data.as_slice(), ZSTD_LEVEL).context("zstd compress frame")?;
        let mut payload = Vec::with_capacity(8 + compressed.len());
        payload.extend_from_slice(&frame.width.to_le_bytes());
        payload.extend_from_slice(&frame.height.to_le_bytes());
        payload.extend_from_slice(&compressed);
        Ok(payload)
    }

    /// Decode a Frame from the wire format.
    fn decode_frame(payload: &[u8]) -> Result<Frame> {
        if payload.len() < 8 {
            anyhow::bail!("frame payload too short: {} bytes", payload.len());
        }
        let width = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let height = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
        let data = zstd::decode_all(&payload[8..]).context("zstd decompress frame")?;
        let expected = (width * height * 4) as usize;
        if data.len() != expected {
            anyhow::bail!(
                "frame size mismatch: got {} bytes, expected {} ({}x{}x4)",
                data.len(),
                expected,
                width,
                height
            );
        }
        Ok(Frame {
            width,
            height,
            format: PixelFormat::Bgra8,
            data,
            timestamp: Instant::now(),
        })
    }

    // ── IPC Server (Service side) ───────────────────────────────────────────

    /// IPC server running in the service process (Session 0).
    /// Creates a named pipe and waits for the agent to connect.
    pub struct IpcServer {
        handle: HANDLE,
        connected: bool,
        frame_rx: Option<mpsc::Receiver<Frame>>,
        input_tx: Option<mpsc::Sender<InputEvent>>,
        shutdown: Arc<AtomicBool>,
        _read_thread: Option<std::thread::JoinHandle<()>>,
        _write_thread: Option<std::thread::JoinHandle<()>>,
    }

    // HANDLE is Send-safe (it's just a pointer-sized value used for kernel calls)
    unsafe impl Send for IpcServer {}

    impl IpcServer {
        /// Create the named pipe and wait for the agent to connect.
        /// Returns immediately after pipe creation; call `wait_for_connection`
        /// to block until an agent connects.
        pub fn new() -> Result<Self> {
            let handle = unsafe {
                let h = CreateNamedPipeW(
                    &HSTRING::from(PIPE_NAME),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    PIPE_BUFFER_SIZE,
                    PIPE_BUFFER_SIZE,
                    0,
                    None,
                );
                if h.is_invalid() {
                    anyhow::bail!("CreateNamedPipe failed: {}", windows::core::Error::from_win32());
                }
                h
            };

            Ok(Self {
                handle,
                connected: false,
                frame_rx: None,
                input_tx: None,
                shutdown: Arc::new(AtomicBool::new(false)),
                _read_thread: None,
                _write_thread: None,
            })
        }

        /// Block until an agent connects to the pipe, or timeout expires.
        /// After connection, spawns read/write threads.
        pub fn wait_for_connection(&mut self, timeout: Duration) -> Result<bool> {
            tracing::info!("IPC: waiting for agent connection (timeout {:?})", timeout);
            let handle = SendHandle(self.handle);
            let (done_tx, done_rx) = mpsc::channel();

            std::thread::Builder::new()
                .name("ipc-connect-wait".into())
                .spawn(move || {
                    let result = unsafe { ConnectNamedPipe(handle.get(), None) };
                    let _ = done_tx.send(result);
                })?;

            match done_rx.recv_timeout(timeout) {
                Ok(result) => {
                    if let Err(ref e) = result {
                        // ERROR_PIPE_CONNECTED (0x80070217): client already connected
                        // before we called ConnectNamedPipe — this is OK.
                        const ERROR_PIPE_CONNECTED: i32 = 0x80070217u32 as i32;
                        if e.code().0 != ERROR_PIPE_CONNECTED {
                            result.context("ConnectNamedPipe")?;
                        }
                    }
                    self.connected = true;
                    self.start_io_threads()?;
                    tracing::info!("IPC: agent connected");
                    Ok(true)
                }
                Err(_) => {
                    tracing::debug!("IPC: connection timed out after {:?}", timeout);
                    Ok(false)
                }
            }
        }

        /// Start read (frame) and write (input) threads.
        fn start_io_threads(&mut self) -> Result<()> {
            let (frame_tx, frame_rx) = mpsc::channel();
            let (input_tx, input_rx) = mpsc::channel::<InputEvent>();

            self.frame_rx = Some(frame_rx);
            self.input_tx = Some(input_tx);

            // Read thread: receives frames from agent
            let handle = SendHandle(self.handle);
            let shutdown = Arc::clone(&self.shutdown);
            let read_thread =
                std::thread::Builder::new()
                    .name("ipc-read".into())
                    .spawn(move || {
                        let handle = handle.get();
                        while !shutdown.load(Ordering::Relaxed) {
                            match unsafe { recv_message(handle) } {
                                Ok((MSG_FRAME, payload)) => {
                                    match decode_frame(&payload) {
                                        Ok(frame) => {
                                            // Use try_send-like behavior: if receiver is behind,
                                            // just drop the frame (latest-wins for video)
                                            let _ = frame_tx.send(frame);
                                        }
                                        Err(e) => {
                                            tracing::warn!("IPC: bad frame: {e}");
                                        }
                                    }
                                }
                                Ok((MSG_HEARTBEAT, _)) => {
                                    tracing::trace!("IPC: heartbeat from agent");
                                }
                                Ok((msg_type, _)) => {
                                    tracing::debug!(
                                        "IPC: unexpected message type 0x{:02x}",
                                        msg_type
                                    );
                                }
                                Err(e) => {
                                    if !shutdown.load(Ordering::Relaxed) {
                                        tracing::warn!("IPC read error (agent disconnected?): {e}");
                                    }
                                    break;
                                }
                            }
                        }
                    })?;

            // Write thread: sends input events to agent
            let handle = SendHandle(self.handle);
            let shutdown = Arc::clone(&self.shutdown);
            let write_thread =
                std::thread::Builder::new()
                    .name("ipc-write".into())
                    .spawn(move || {
                        let handle = handle.get();
                        while !shutdown.load(Ordering::Relaxed) {
                            match input_rx.recv_timeout(Duration::from_secs(5)) {
                                Ok(event) => {
                                    let payload = match bincode::serialize(&event) {
                                        Ok(p) => p,
                                        Err(e) => {
                                            tracing::warn!("IPC: serialize input: {e}");
                                            continue;
                                        }
                                    };
                                    if let Err(e) =
                                        unsafe { send_message(handle, MSG_INPUT, &payload) }
                                    {
                                        if !shutdown.load(Ordering::Relaxed) {
                                            tracing::warn!("IPC write error: {e}");
                                        }
                                        break;
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {
                                    // Send heartbeat to detect broken pipe
                                    if let Err(e) =
                                        unsafe { send_message(handle, MSG_HEARTBEAT, &[]) }
                                    {
                                        if !shutdown.load(Ordering::Relaxed) {
                                            tracing::warn!("IPC heartbeat write error: {e}");
                                        }
                                        break;
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                    })?;

            self._read_thread = Some(read_thread);
            self._write_thread = Some(write_thread);
            Ok(())
        }

        /// Try to receive the latest frame from the agent.
        /// Returns the most recent frame, dropping any older queued frames.
        pub fn recv_frame(&self) -> Option<Frame> {
            let rx = self.frame_rx.as_ref()?;
            let mut latest = None;
            // Drain to get the most recent frame (skip stale ones)
            while let Ok(frame) = rx.try_recv() {
                latest = Some(frame);
            }
            latest
        }

        /// Send an input event to the agent for injection.
        pub fn send_input(&self, event: InputEvent) -> Result<()> {
            if let Some(ref tx) = self.input_tx {
                tx.send(event).context("IPC input channel closed")?;
            }
            Ok(())
        }

        /// Get a cloneable input sender for use as an InputForwarder.
        /// Returns None if IPC I/O threads haven't been started.
        pub fn input_sender(&self) -> Option<mpsc::Sender<InputEvent>> {
            self.input_tx.clone()
        }

        /// Send shutdown command to agent.
        pub fn send_shutdown(&self) -> Result<()> {
            if self.connected {
                unsafe { send_message(self.handle, MSG_SHUTDOWN, &[]) }
            } else {
                Ok(())
            }
        }

        /// Whether the agent is connected.
        pub fn is_connected(&self) -> bool {
            self.connected
        }

        /// Disconnect and clean up.
        pub fn disconnect(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            if self.connected {
                let _ = self.send_shutdown();
                unsafe {
                    let _ = DisconnectNamedPipe(self.handle);
                }
                self.connected = false;
            }
            self.frame_rx = None;
            self.input_tx = None;
        }
    }

    impl Drop for IpcServer {
        fn drop(&mut self) {
            self.disconnect();
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }

    // ── IPC Client (Agent side) ─────────────────────────────────────────────

    /// IPC client running in the agent process (user session).
    /// Connects to the service's named pipe.
    pub struct IpcClient {
        handle: HANDLE,
        shutdown: Arc<AtomicBool>,
        input_rx: Option<mpsc::Receiver<InputEvent>>,
        _read_thread: Option<std::thread::JoinHandle<()>>,
    }

    unsafe impl Send for IpcClient {}

    impl IpcClient {
        /// Connect to the service's named pipe.
        pub fn connect() -> Result<Self> {
            // Retry connecting to the pipe — service may not have created it yet.
            let handle = {
                let mut last_err = None;
                let mut h = None;
                for attempt in 0..50u32 {
                    match unsafe {
                        CreateFileW(
                            &HSTRING::from(PIPE_NAME),
                            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                            FILE_SHARE_NONE,
                            None,
                            OPEN_EXISTING,
                            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
                            None,
                        )
                    } {
                        Ok(handle) => {
                            tracing::info!("IPC: connected to pipe on attempt {}", attempt + 1);
                            h = Some(handle);
                            break;
                        }
                        Err(e) => {
                            if attempt < 49 {
                                std::thread::sleep(std::time::Duration::from_millis(200));
                            }
                            last_err = Some(e);
                        }
                    }
                }
                match h {
                    Some(handle) => handle,
                    None => return Err(last_err.unwrap()).context("connect to IPC pipe after 50 attempts"),
                }
            };

            let shutdown = Arc::new(AtomicBool::new(false));

            // Read thread: receives input events and shutdown from service
            let (input_tx, input_rx) = mpsc::channel();
            let read_handle = SendHandle(handle);
            let read_shutdown = Arc::clone(&shutdown);

            let read_thread = std::thread::Builder::new()
                .name("ipc-agent-read".into())
                .spawn(move || {
                    let read_handle = read_handle.get();
                    while !read_shutdown.load(Ordering::Relaxed) {
                        match unsafe { recv_message(read_handle) } {
                            Ok((MSG_INPUT, payload)) => {
                                match bincode::deserialize::<InputEvent>(&payload) {
                                    Ok(event) => {
                                        let _ = input_tx.send(event);
                                    }
                                    Err(e) => {
                                        tracing::warn!("IPC: deserialize input: {e}");
                                    }
                                }
                            }
                            Ok((MSG_SHUTDOWN, _)) => {
                                tracing::info!("IPC: received shutdown from service");
                                read_shutdown.store(true, Ordering::SeqCst);
                                break;
                            }
                            Ok((MSG_HEARTBEAT, _)) => {
                                tracing::trace!("IPC: heartbeat from service");
                            }
                            Ok((msg_type, _)) => {
                                tracing::debug!("IPC: unexpected message 0x{:02x}", msg_type);
                            }
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

            tracing::info!("IPC: connected to service pipe");
            Ok(Self {
                handle,
                shutdown,
                input_rx: Some(input_rx),
                _read_thread: Some(read_thread),
            })
        }

        /// Send a captured frame to the service.
        pub fn send_frame(&self, frame: &Frame) -> Result<()> {
            let payload = encode_frame(frame)?;
            unsafe { send_message(self.handle, MSG_FRAME, &payload) }
        }

        /// Try to receive pending input events from the service.
        /// Returns all queued events (non-blocking).
        pub fn recv_inputs(&self) -> Vec<InputEvent> {
            let mut events = Vec::new();
            if let Some(ref rx) = self.input_rx {
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
            }
            events
        }

        /// Whether the service has requested shutdown.
        pub fn should_shutdown(&self) -> bool {
            self.shutdown.load(Ordering::Relaxed)
        }
    }

    impl Drop for IpcClient {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

// ── Non-Windows stubs ───────────────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
mod platform {
    use anyhow::Result;
    use phantom_core::frame::Frame;
    use phantom_core::input::InputEvent;
    use std::time::Duration;

    pub struct IpcServer;
    impl IpcServer {
        pub fn new() -> Result<Self> {
            anyhow::bail!("IPC pipes are only supported on Windows")
        }
        pub fn wait_for_connection(&mut self, _timeout: Duration) -> Result<bool> {
            Ok(false)
        }
        pub fn recv_frame(&self) -> Option<Frame> {
            None
        }
        pub fn send_input(&self, _event: InputEvent) -> Result<()> {
            Ok(())
        }
        pub fn input_sender(&self) -> Option<std::sync::mpsc::Sender<InputEvent>> {
            None
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
        pub fn connect() -> Result<Self> {
            anyhow::bail!("IPC pipes are only supported on Windows")
        }
        pub fn send_frame(&self, _frame: &Frame) -> Result<()> {
            Ok(())
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
pub use platform::{IpcClient, IpcServer};
