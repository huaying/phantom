//! Windows Service integration for Phantom server.
//!
//! When installed as a Windows Service, the server runs as LocalSystem in Session 0,
//! starting at boot — before any user logs in. This enables remote access to the
//! lock screen and login screen.
//!
//! Architecture (like Sunshine/RustDesk):
//! - Service (Session 0): handles network connections, manages agent lifecycle.
//!   Polls `WTSGetActiveConsoleSessionId()` until a console session appears
//!   (Session 1+ is created by winlogon a few seconds after boot).
//! - Agent (User Session): launched via CreateProcessAsUser into the active
//!   console session. Uses `OpenInputDesktop()` + `SetThreadDesktop()` each
//!   frame to follow desktop switches (user desktop ↔ Winlogon lock screen).
//!   Handles DXGI capture + input injection in the interactive desktop.
//! - No Session 0 capture: GDI/DXGI cannot capture cross-session desktops.
//!   The service waits for the agent to be ready before serving frames.

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Debug logger for Windows Service (no stderr available).
pub fn svc_log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"C:\Windows\Temp\phantom-debug.log")
    {
        let _ = writeln!(
            f,
            "[{:.1}s] {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            msg
        );
    }
}
use std::time::Duration;
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;

const SERVICE_NAME: &str = "PhantomServer";
const SERVICE_DISPLAY_NAME: &str = "Phantom Remote Desktop Server";
const SERVICE_DESCRIPTION: &str =
    "Phantom remote desktop server — provides remote access including pre-login lock screen.";

/// Entry point when invoked by the Windows Service Control Manager.
/// Call this from main() when `--service` flag is passed.
pub fn run_as_service() -> Result<(), windows_service::Error> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

// The service main function called by SCM via the dispatcher.
// windows-service requires this exact signature.
define_windows_service!(ffi_service_main, phantom_service_main);

fn phantom_service_main(arguments: Vec<OsString>) {
    if let Err(e) = run_service(arguments) {
        tracing::error!("Service failed: {e}");
    }
}

fn run_service(_arguments: Vec<OsString>) -> anyhow::Result<()> {
    tracing::info!("=== Service starting ===");

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    // Cancel flag shared with active session — Stop handler sets this
    // to break out of create_service_session's blocking loop.
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let cancel_clone = Arc::clone(&cancel);

    // Register the service control handler.
    let session_changed = Arc::new(AtomicBool::new(false));
    let session_changed_clone = Arc::clone(&session_changed);

    let status_handle = service_control_handler::register(
        SERVICE_NAME,
        move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    shutdown_clone.store(true, Ordering::SeqCst);
                    // Also cancel any active session so main loop unblocks
                    cancel_clone.store(true, Ordering::SeqCst);
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::SessionChange(_) => {
                    // A user logged in/out — wake the session manager
                    session_changed_clone.store(true, Ordering::Relaxed);
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        },
    )?;

    // Report "Running" to SCM
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Run the actual server logic.
    let result = run_server_loop(Arc::clone(&shutdown), session_changed, cancel);

    if let Err(ref e) = result {
        tracing::error!("Server loop error: {e}");
    }

    // Report "Stopped" to SCM
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(if result.is_ok() { 0 } else { 1 }),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

/// Main server loop when running as a service.
/// Uses the same transport/session infrastructure as console mode.
fn run_server_loop(
    shutdown: Arc<AtomicBool>,
    session_changed: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    use crate::transport_tcp;
    use crate::transport_ws;
    use phantom_core::transport::{MessageReceiver, MessageSender};
    use std::sync::mpsc;

    type ConnectionPair = (Box<dyn MessageSender>, Box<dyn MessageReceiver>);

    let listen_addr = "0.0.0.0:9900";
    let base_port: u16 = 9900;

    // Start TCP listener
    let tcp_listener = transport_tcp::TcpServerTransport::bind(listen_addr)?;
    let (conn_tx, conn_rx) = mpsc::channel::<ConnectionPair>();

    let tx = conn_tx.clone();
    std::thread::Builder::new()
        .name("svc-tcp-accept".into())
        .spawn(move || loop {
            match tcp_listener.accept_tcp() {
                Ok(conn) => {
                    // No encryption in service mode by default
                    match conn.split() {
                        Ok((s, r)) => {
                            let pair = (
                                Box::new(s) as Box<dyn MessageSender>,
                                Box::new(r) as Box<dyn MessageReceiver>,
                            );
                            if tx.send(pair).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("TCP split failed: {e}");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("TCP accept error: {e}");
                }
            }
        })?;

    // Start Web/WS listener
    // Read JWT auth secret from environment variable (hex string)
    let auth_secret: Option<Vec<u8>> = std::env::var("PHANTOM_AUTH_SECRET").ok().and_then(|hex| {
        let bytes: Result<Vec<u8>, _> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect();
        match bytes {
            Ok(b) => {
                tracing::info!("JWT authentication ENABLED ({} byte secret)", b.len());
                Some(b)
            }
            Err(_) => {
                tracing::warn!("PHANTOM_AUTH_SECRET invalid hex, auth disabled");
                None
            }
        }
    });
    let ws_transport = transport_ws::WebServerTransport::start(
        base_port + 1,
        base_port + 2,
        base_port + 3,
        auth_secret,
    )?;
    let tx = conn_tx.clone();
    std::thread::Builder::new()
        .name("svc-web-accept".into())
        .spawn(move || loop {
            match ws_transport.accept_ws() {
                Ok(pair) => {
                    if tx.send(pair).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("WebSocket accept error: {e}");
                }
            }
        })?;
    drop(conn_tx);

    // Session manager: monitors user sessions and launches agents
    svc_log("Creating SessionManager");
    let mut session_mgr = SessionManager::new();
    svc_log("Initial session update");
    session_mgr.update();
    svc_log(&format!(
        "After update: agent={} ipc={}",
        session_mgr.agent.is_some(),
        session_mgr.ipc.is_some()
    ));
    tracing::info!(
        has_agent = session_mgr.agent.is_some(),
        has_ipc = session_mgr.ipc.is_some(),
        "After initial session update"
    );

    // Main loop: accept connections and run sessions
    let pending: Arc<std::sync::Mutex<Option<ConnectionPair>>> =
        Arc::new(std::sync::Mutex::new(None));
    // cancel: shared with Stop handler (passed from run_service)
    let conn_rx = Arc::new(std::sync::Mutex::new(conn_rx));

    // Doorbell thread
    {
        let conn_rx = Arc::clone(&conn_rx);
        let pending = Arc::clone(&pending);
        let cancel = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("svc-doorbell".into())
            .spawn(move || loop {
                let pair = { conn_rx.lock().unwrap().recv() };
                match pair {
                    Ok(conn) => {
                        let had_existing = pending.lock().unwrap().is_some();
                        *pending.lock().unwrap() = Some(conn);
                        if had_existing {
                            tracing::info!("New client arrived, replacing queued connection");
                        }
                        cancel.store(true, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            })?;
    }

    while !shutdown.load(Ordering::Relaxed) {
        // Check for session changes (driven by SCM SESSION_CHANGE events).
        // Also poll when agent/IPC are not ready — like Sunshine, we poll every
        // iteration (~100ms) until an agent is connected. This handles:
        // - Boot race: service starts before winlogon creates Session 1
        // - Missed SESSION_CHANGE events
        // - Agent crash recovery
        if session_changed.swap(false, Ordering::Relaxed)
            || session_mgr.agent.is_none()
            || session_mgr.ipc.is_none()
        {
            session_mgr.update();
        }
        // Also check if agent died unexpectedly
        session_mgr.check_agent_health();

        // Check for pending connection
        let conn = pending.lock().unwrap().take();
        if let Some((sender, receiver)) = conn {
            cancel.store(false, Ordering::Relaxed);
            let session_cancel = Arc::clone(&cancel);

            // In service mode, relay agent frames via IPC (DXGI/GDI from user session).
            // If agent is not connected yet, reject the client.
            match create_service_session(&mut session_mgr, sender, receiver, session_cancel) {
                Ok(result) => {
                    tracing::info!("Service session ended: {}", result.error);
                }
                Err(e) => {
                    tracing::error!("Service session failed: {e}");
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    // Shutdown: kill agent if running
    session_mgr.kill_agent();

    Ok(())
}

/// Run a service-mode session with IPC agent frame proxying.
///
/// The agent (running in the user's session) captures the screen via DXGI/GDI
/// and sends pre-encoded H.264 frames over IPC. The service just relays them.
/// If the agent is not connected, returns an error — Session 0 cannot capture
/// cross-session desktops (Windows Vista+ session isolation).
fn create_service_session(
    session_mgr: &mut SessionManager,
    sender: Box<dyn phantom_core::transport::MessageSender>,
    receiver: Box<dyn phantom_core::transport::MessageReceiver>,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<crate::session::SessionResult> {
    let frame_interval = Duration::from_secs_f64(1.0 / 30.0);

    // Check if agent IPC is available
    #[cfg(target_os = "windows")]
    let has_ipc = session_mgr.ipc().is_some();
    #[cfg(not(target_os = "windows"))]
    let has_ipc = false;

    svc_log(&format!("create_service_session: has_ipc={has_ipc}"));
    if !has_ipc {
        anyhow::bail!(
            "no agent connected — cannot capture screen from Session 0 \
             (waiting for user session agent to connect)"
        );
    }

    #[cfg(target_os = "windows")]
    {
        let ipc = session_mgr.ipc.as_ref().unwrap();

        // Wait for first encoded frame from agent to get resolution
        let mut attempts = 0;
        svc_log("Waiting for first encoded frame from agent...");
        let (width, height) = loop {
            if let Some(ef) = ipc.recv_encoded_frames().into_iter().next() {
                svc_log(&format!(
                    "Got frame: {}x{} {} bytes kf={}",
                    ef.width,
                    ef.height,
                    ef.encoded.data.len(),
                    ef.encoded.is_keyframe
                ));
                tracing::info!(
                    width = ef.width,
                    height = ef.height,
                    bytes = ef.encoded.data.len(),
                    keyframe = ef.encoded.is_keyframe,
                    "Got encoded frame from agent"
                );
                break (ef.width, ef.height);
            }
            attempts += 1;
            if attempts % 10 == 0 {
                tracing::debug!("Still waiting for agent frame... attempt {attempts}/100");
            }
            if attempts > 100 {
                svc_log("No frames after 2s — agent not producing frames");
                anyhow::bail!("agent connected but not producing frames after 2s");
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        // Create input forwarder to send input events to agent via IPC
        let input_forwarder: Option<Box<dyn crate::session::InputForwarder>> =
            ipc.input_sender().map(|tx| {
                Box::new(IpcInputForwarder { tx }) as Box<dyn crate::session::InputForwarder>
            });

        let result = crate::session::run_session_ipc(
            ipc,
            crate::session::SessionConfig {
                sender,
                receiver,
                frame_interval,
                quality_delay: Duration::from_millis(2000),
                cancel,
                send_file: None,
                video_codec: phantom_core::encode::VideoCodec::H264,
                is_resume: false,
                input_forwarder,
                audio_ws_rx: None,
            },
            width,
            height,
        );
        return Ok(result);
    }

    #[cfg(not(target_os = "windows"))]
    unreachable!()
}

/// An InputForwarder that sends input events to the agent via IPC.
#[cfg(target_os = "windows")]
struct IpcInputForwarder {
    tx: std::sync::mpsc::Sender<phantom_core::input::InputEvent>,
}

#[cfg(target_os = "windows")]
impl crate::session::InputForwarder for IpcInputForwarder {
    fn forward_input(&self, event: &phantom_core::input::InputEvent) -> anyhow::Result<()> {
        self.tx
            .send(event.clone())
            .map_err(|e| anyhow::anyhow!("IPC input forward failed: {e}"))
    }
}

// ── Process Handle Wrapper ──────────────────────────────────────────────────

/// Wrapper around a raw Win32 process handle from CreateProcessAsUser.
/// Provides kill/wait/try_wait similar to std::process::Child but works
/// with processes created via CreateProcessAsUser (which can't produce
/// a std::process::Child).
#[cfg(target_os = "windows")]
struct WinProcessHandle {
    handle: windows::Win32::Foundation::HANDLE,
    pid: u32,
}

#[cfg(target_os = "windows")]
impl WinProcessHandle {
    /// Terminate the process.
    fn kill(&self) {
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(self.handle, 1);
        }
    }

    /// Check if the process has exited. Returns Some(exit_code) if exited.
    fn try_wait(&self) -> Option<u32> {
        if self.handle.is_invalid() || self.pid == 0 {
            return None; // No handle (schtasks-launched agent)
        }
        unsafe {
            use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
            let wait_result = WaitForSingleObject(self.handle, 0);
            if wait_result.0 == 258 {
                return None;
            }
            let mut exit_code: u32 = 0;
            let _ = GetExitCodeProcess(self.handle, &mut exit_code);
            Some(exit_code)
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for WinProcessHandle {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}

// ── Session Manager ─────────────────────────────────────────────────────────

/// Monitors Windows user sessions and manages the agent process lifecycle.
/// When a user logs in, it launches a phantom agent in their session
/// and establishes an IPC pipe for frame/input proxying.
struct SessionManager {
    #[cfg(target_os = "windows")]
    agent: Option<WinProcessHandle>,
    #[cfg(target_os = "windows")]
    current_session_id: u32,
    #[cfg(target_os = "windows")]
    ipc: Option<crate::ipc_pipe::IpcServer>,
}

impl SessionManager {
    fn new() -> Self {
        Self {
            #[cfg(target_os = "windows")]
            agent: None,
            #[cfg(target_os = "windows")]
            current_session_id: 0,
            #[cfg(target_os = "windows")]
            ipc: None,
        }
    }

    /// React to a session change event. Check current active session and
    /// launch/kill agent as needed.
    ///
    /// Called periodically from the main loop. Handles:
    /// - Session transitions (logon/logoff/lock/unlock)
    /// - Boot race condition (Session 1 not yet created when service starts)
    /// - Agent recovery after crash
    fn update(&mut self) {
        #[cfg(target_os = "windows")]
        {
            let session_id = get_active_console_session_id();
            svc_log(&format!(
                "update: current={} detected={} agent={} ipc={}",
                self.current_session_id,
                session_id,
                self.agent.is_some(),
                self.ipc.is_some()
            ));

            if session_id == self.current_session_id && self.agent.is_some() && self.ipc.is_some() {
                return; // Session unchanged and agent is healthy — nothing to do
            }

            // Session changed — kill old agent before launching new one
            if session_id != self.current_session_id {
                svc_log(&format!(
                    "Session changed: {} -> {}",
                    self.current_session_id, session_id
                ));
                self.current_session_id = session_id;
                self.kill_agent();
            }

            // No valid session yet (boot race: winlogon hasn't created Session 1)
            if session_id == 0xFFFFFFFF || session_id == 0 {
                return;
            }

            // Already have a working agent — nothing to do
            if self.agent.is_some() && self.ipc.is_some() {
                return;
            }

            // Need to launch agent (first time, or after crash/session change)
            if self.agent.is_some() {
                // Agent exists but IPC is broken — kill and relaunch
                svc_log("Agent exists but IPC disconnected, relaunching");
                self.kill_agent();
            }

            {
                svc_log(&format!("Creating IPC pipe for session {session_id}"));
                match crate::ipc_pipe::IpcServer::new(session_id) {
                    Ok(mut ipc_server) => {
                        svc_log("IPC pipe created, launching agent");
                        match launch_agent_in_session(session_id) {
                            Ok(proc) => {
                                svc_log(&format!("Agent launched PID={}", proc.pid));
                                self.agent = Some(proc);

                                // Wait for agent to connect to the IPC pipe (up to 10s)
                                svc_log("Waiting for agent IPC connection (10s timeout)...");
                                match ipc_server.wait_for_connection(Duration::from_secs(10)) {
                                    Ok(true) => {
                                        svc_log("IPC: agent connected!");
                                        self.ipc = Some(ipc_server);
                                    }
                                    Ok(false) => {
                                        svc_log("IPC: agent did not connect within timeout");
                                        ipc_server.disconnect();
                                    }
                                    Err(e) => {
                                        svc_log(&format!("IPC: connection error: {e}"));
                                        ipc_server.disconnect();
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(session_id, "Failed to launch agent: {e}");
                                ipc_server.disconnect();
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to create IPC pipe: {e}");
                        // Still try to launch agent (it will fail to connect but that's OK)
                        match launch_agent_in_session(session_id) {
                            Ok(proc) => {
                                tracing::info!(
                                    session_id,
                                    pid = proc.pid,
                                    "Launched agent (no IPC)"
                                );
                                self.agent = Some(proc);
                            }
                            Err(e) => {
                                tracing::error!(session_id, "Failed to launch agent: {e}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// Check if the agent process is still alive. If it died, clean up state.
    /// The next `update()` call will detect `agent.is_none()` and relaunch.
    fn check_agent_health(&mut self) {
        #[cfg(target_os = "windows")]
        {
            if let Some(ref agent) = self.agent {
                if let Some(exit_code) = agent.try_wait() {
                    tracing::warn!(pid = agent.pid, exit_code, "Agent exited unexpectedly");
                    svc_log(&format!(
                        "Agent PID={} exited with code {exit_code}",
                        agent.pid
                    ));
                    self.agent = None;
                    // Clean up IPC — next update() will relaunch
                    if let Some(ref mut ipc) = self.ipc {
                        ipc.disconnect();
                    }
                    self.ipc = None;
                }
            }
        }
    }

    /// Kill the current agent process if one is running.
    fn kill_agent(&mut self) {
        #[cfg(target_os = "windows")]
        {
            // Disconnect IPC first (sends shutdown to agent)
            if let Some(ref mut ipc) = self.ipc {
                ipc.disconnect();
            }
            self.ipc = None;

            if let Some(agent) = self.agent.take() {
                tracing::info!(pid = agent.pid, "Killing agent");
                agent.kill();
                // Handle is closed on drop
            }
        }
    }

    /// Get a reference to the IPC server (if agent is connected).
    #[cfg(target_os = "windows")]
    fn ipc(&self) -> Option<&crate::ipc_pipe::IpcServer> {
        self.ipc.as_ref().filter(|ipc| ipc.is_connected())
    }
}

// ── Windows-specific session management functions ───────────────────────────

/// Get the session ID of the active console session.
#[cfg(target_os = "windows")]
fn get_active_console_session_id() -> u32 {
    extern "system" {
        fn WTSGetActiveConsoleSessionId() -> u32;
    }
    unsafe { WTSGetActiveConsoleSessionId() }
}

/// Launch the phantom agent process in a specific user session.
/// Get the username associated with a Windows session ID.
#[cfg(target_os = "windows")]
fn get_session_username(session_id: u32) -> Option<String> {
    let output = std::process::Command::new("query")
        .args(["session"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        // Format: " SESSIONNAME  USERNAME  ID  STATE  TYPE  DEVICE"
        let parts: Vec<&str> = line.split_whitespace().collect();
        // Find the line where session ID matches
        for (i, part) in parts.iter().enumerate() {
            if let Ok(id) = part.parse::<u32>() {
                if id == session_id && i >= 2 {
                    let username = parts[i - 1];
                    if !username.is_empty() && username != "services" && username != "console" {
                        return Some(username.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Launch agent in the target session using SYSTEM token + CreateProcessAsUser.
///
/// Uses the service's own SYSTEM token with the session ID set to the target
/// session. This gives the agent SYSTEM privileges, allowing access to both
/// user desktop and Winlogon desktop (lock screen).
///
/// Returns a WinProcessHandle that owns the process handle for lifecycle management.
#[cfg(target_os = "windows")]
fn launch_agent_in_session(session_id: u32) -> anyhow::Result<WinProcessHandle> {
    use anyhow::Context;
    use std::mem;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{
        CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, CREATE_UNICODE_ENVIRONMENT,
        PROCESS_INFORMATION, STARTUPINFOW,
    };

    unsafe {
        let mut service_token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ALL_ACCESS | TOKEN_QUERY,
            &mut service_token,
        )
        .context("OpenProcessToken failed")?;

        let mut dup_token = HANDLE::default();
        let dup_result = DuplicateTokenEx(
            service_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut dup_token,
        );
        let _ = CloseHandle(service_token);
        dup_result.context("DuplicateTokenEx (SYSTEM token) failed")?;

        // Set the session ID on the duplicated token
        let sid = session_id;
        let result = windows::Win32::Security::SetTokenInformation(
            dup_token,
            windows::Win32::Security::TokenSessionId,
            &sid as *const u32 as *const std::ffi::c_void,
            mem::size_of::<u32>() as u32,
        );
        if result.is_err() {
            let _ = CloseHandle(dup_token);
            anyhow::bail!("SetTokenInformation(TokenSessionId={session_id}) failed");
        }

        // Build user's environment block
        let mut env_block: *mut std::ffi::c_void = std::ptr::null_mut();
        let _ = windows::Win32::System::Environment::CreateEnvironmentBlock(
            &mut env_block,
            dup_token,
            false,
        );

        let exe_path = std::env::current_exe().context("get current exe")?;
        let cmd_line = format!(
            "\"{}\" --agent-mode --ipc-session {} --listen 127.0.0.1:9910 --no-encrypt",
            exe_path.display(),
            session_id,
        );
        let mut cmd_wide: Vec<u16> = cmd_line.encode_utf16().chain(std::iter::once(0)).collect();

        let mut si: STARTUPINFOW = mem::zeroed();
        si.cb = mem::size_of::<STARTUPINFOW>() as u32;
        let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        si.lpDesktop = windows::core::PWSTR(desktop.as_mut_ptr());

        let mut pi: PROCESS_INFORMATION = mem::zeroed();

        let result = CreateProcessAsUserW(
            dup_token,
            None,
            windows::core::PWSTR(cmd_wide.as_mut_ptr()),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT,
            if env_block.is_null() {
                None
            } else {
                Some(env_block)
            },
            None,
            &si,
            &mut pi,
        );
        let _ = CloseHandle(dup_token);
        if !env_block.is_null() {
            let _ = windows::Win32::System::Environment::DestroyEnvironmentBlock(env_block);
        }
        result.context("CreateProcessAsUserW failed")?;

        let _ = CloseHandle(pi.hThread);

        Ok(WinProcessHandle {
            handle: pi.hProcess,
            pid: pi.dwProcessId,
        })
    }
}

// ── Service installation helpers ────────────────────────────────────────────

/// Install Phantom as a Windows Service (replaces schtasks approach).
///
/// Uses `sc.exe` to create a service that runs as LocalSystem at boot.
/// The `--service` flag in binPath tells the server to enter SCM dispatcher mode.
pub fn install_service() -> anyhow::Result<()> {
    use anyhow::Context;

    // Copy exe to a fixed install location. This avoids binPath being tied
    // to the build directory — updates just overwrite the fixed path.
    let install_dir = std::path::PathBuf::from(r"C:\Program Files\Phantom");
    std::fs::create_dir_all(&install_dir).context("create install dir")?;
    let install_exe = install_dir.join("phantom-server.exe");

    let src_exe = std::env::current_exe().context("get current exe path")?;
    std::fs::copy(&src_exe, &install_exe)
        .context("copy exe to install dir (is the service already running? --uninstall first)")?;

    let bin_path = format!("\"{}\" --service", install_exe.display());
    let status = std::process::Command::new("sc")
        .args([
            "create",
            SERVICE_NAME,
            "binPath=",
            &bin_path,
            "start=",
            "auto",
            "obj=",
            "LocalSystem",
            "DisplayName=",
            SERVICE_DISPLAY_NAME,
        ])
        .status()
        .context("sc create")?;

    if !status.success() {
        anyhow::bail!("sc create failed with {status}. Run as Administrator.");
    }

    // Set description
    let _ = std::process::Command::new("sc")
        .args(["description", SERVICE_NAME, SERVICE_DESCRIPTION])
        .status();

    // Configure service recovery: restart on failure (5s, 10s, 30s)
    let _ = std::process::Command::new("sc")
        .args([
            "failure",
            SERVICE_NAME,
            "reset=",
            "86400",
            "actions=",
            "restart/5000/restart/10000/restart/30000",
        ])
        .status();

    println!("Installed: {SERVICE_DISPLAY_NAME} (Windows Service)");
    println!("  The service runs at boot as LocalSystem (Session 0).");
    println!("  Remote access works even before user login.");
    println!();

    // Start the service
    let start_status = std::process::Command::new("sc")
        .args(["start", SERVICE_NAME])
        .status()
        .context("sc start")?;

    if start_status.success() {
        println!("  Service started successfully.");
    } else {
        println!("  Service created but could not start (start manually or reboot).");
    }

    println!("  To check status: sc query {SERVICE_NAME}");
    println!("  To remove:       phantom-server --uninstall");

    Ok(())
}

/// Uninstall the Phantom Windows Service.
pub fn uninstall_service() -> anyhow::Result<()> {
    use anyhow::Context;

    // Stop the service gracefully
    let _ = std::process::Command::new("sc")
        .args(["stop", SERVICE_NAME])
        .status();

    // Wait for graceful stop (Bug 1 fix: Stop handler now cancels active session)
    std::thread::sleep(Duration::from_secs(5));

    // Force kill fallback — if sc stop didn't work, kill the process directly
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/FI", &format!("SERVICES eq {SERVICE_NAME}")])
        .status();
    // Also kill any agent processes
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", "phantom-server.exe"])
        .status();

    std::thread::sleep(Duration::from_secs(2));

    // Delete service
    let status = std::process::Command::new("sc")
        .args(["delete", SERVICE_NAME])
        .status()
        .context("sc delete")?;

    if status.success() {
        println!("Removed: {SERVICE_DISPLAY_NAME} (Windows Service)");
    } else {
        anyhow::bail!("sc delete failed with {status}. Run as Administrator.");
    }

    // Clean up schtasks
    let _ = std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", "PhantomServer", "/F"])
        .status();

    Ok(())
}
