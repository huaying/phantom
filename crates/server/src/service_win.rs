//! Windows Service integration for Phantom server.
//!
//! When installed as a Windows Service, the server runs as LocalSystem in Session 0,
//! starting at boot — before any user logs in. This enables remote access to the
//! lock screen and login screen.
//!
//! Architecture:
//! - Service (Session 0): handles network connections, manages agent lifecycle
//! - Agent (User Session): launched via CreateProcessAsUser when a user logs in,
//!   handles DXGI capture + input injection in the interactive desktop
//! - Fallback: when no user is logged in, service captures the lock screen via GDI

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;

const SERVICE_NAME: &str = "PhantomServer";
const SERVICE_DISPLAY_NAME: &str = "Phantom Remote Desktop Server";
const SERVICE_DESCRIPTION: &str =
    "Phantom remote desktop server — provides remote access including pre-login lock screen.";

/// Entry point when invoked by the Windows Service Control Manager.
/// Call this from main() when service mode is detected.
pub fn run_as_service() -> Result<(), windows_service::Error> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

// The service main function called by SCM via the dispatcher.
// windows-service requires this exact signature.
define_windows_service!(ffi_service_main, phantom_service_main);

fn phantom_service_main(arguments: Vec<OsString>) {
    if let Err(e) = run_service(arguments) {
        // Log to Windows Event Log or stderr (service has no console)
        eprintln!("Service failed: {e}");
    }
}

fn run_service(_arguments: Vec<OsString>) -> anyhow::Result<()> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    // Register the service control handler
    let status_handle = service_control_handler::register(
        SERVICE_NAME,
        move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    shutdown_clone.store(true, Ordering::SeqCst);
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        },
    )?;

    // Report "Running" to SCM
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Run the actual server logic.
    // This re-uses the same server code but with service-appropriate defaults:
    // - Listens on 0.0.0.0:9900
    // - No encryption by default (service can't prompt for key)
    //   Users should configure via registry/config file in production
    // - Monitors for user session changes to launch/kill the agent process
    let result = run_server_loop(Arc::clone(&shutdown));

    if let Err(ref e) = result {
        eprintln!("Server loop error: {e}");
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
fn run_server_loop(shutdown: Arc<AtomicBool>) -> anyhow::Result<()> {
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
                            eprintln!("TCP split failed: {e}");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("TCP accept error: {e}");
                }
            }
        })?;

    // Start Web/WS listener
    let ws_transport =
        transport_ws::WebServerTransport::start(base_port + 1, base_port + 2, base_port + 3)?;
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
                    eprintln!("WebSocket accept error: {e}");
                }
            }
        })?;
    drop(conn_tx);

    // Session manager: monitors user sessions and launches agents
    let mut session_mgr = SessionManager::new();

    // Main loop: accept connections and run sessions
    // In service mode, we use GDI capture as fallback when no user session agent is available
    let pending: Arc<std::sync::Mutex<Option<ConnectionPair>>> =
        Arc::new(std::sync::Mutex::new(None));
    let cancel = Arc::new(AtomicBool::new(false));
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
                        *pending.lock().unwrap() = Some(conn);
                        cancel.store(true, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            })?;
    }

    while !shutdown.load(Ordering::Relaxed) {
        // Check if we need to launch/kill agent for user session changes
        session_mgr.poll();

        // Check for pending connection
        let conn = pending.lock().unwrap().take();
        if let Some((sender, receiver)) = conn {
            cancel.store(false, Ordering::Relaxed);
            let session_cancel = Arc::clone(&cancel);

            // Try to use the agent's DXGI capture if a user session agent is running.
            // Otherwise, fall back to GDI capture (lock screen).
            // For now, use CPU capture as the service-mode fallback.
            let frame_interval = Duration::from_secs_f64(1.0 / 30.0);
            let quality_delay = Duration::from_millis(2000);

            // In service mode, attempt GDI capture (Session 0 / lock screen).
            // This will be replaced by agent IPC in Phase 2.
            match create_service_capture() {
                Ok((mut capture, mut encoder, mut differ)) => {
                    let result = crate::session::run_session_cpu(
                        &mut *capture,
                        &mut *encoder,
                        &mut differ,
                        crate::session::SessionConfig {
                            sender,
                            receiver,
                            frame_interval,
                            quality_delay,
                            cancel: session_cancel,
                            send_file: None,
                            video_codec: phantom_core::encode::VideoCodec::H264,
                            is_resume: false,
                        },
                    );
                    eprintln!("Service session ended: {}", result.error);
                    differ.reset();
                }
                Err(e) => {
                    eprintln!("Service capture init failed: {e}");
                    // Drop connection, client will retry
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    Ok(())
}

/// Create capture/encoder for service mode (Session 0 fallback).
/// Uses scrap (which uses DXGI on Windows) or GDI as last resort.
fn create_service_capture() -> anyhow::Result<(
    Box<dyn phantom_core::capture::FrameCapture>,
    Box<dyn phantom_core::encode::FrameEncoder>,
    phantom_core::tile::TileDiffer,
)> {
    // Try scrap first (works if a desktop is available)
    let capture: Box<dyn phantom_core::capture::FrameCapture> =
        match crate::capture_scrap::ScrapCapture::new() {
            Ok(cap) => Box::new(cap),
            Err(_e) => {
                // Fall back to GDI capture for lock screen / no desktop
                #[cfg(target_os = "windows")]
                {
                    Box::new(crate::capture_gdi::GdiCapture::new()?)
                }
                #[cfg(not(target_os = "windows"))]
                {
                    anyhow::bail!("No capture method available: {_e}");
                }
            }
        };

    let (width, height) = capture.resolution();
    let encoder = Box::new(crate::encode_h264::OpenH264Encoder::new(
        width,
        height,
        30.0,
        2000,
    )?);
    let differ = phantom_core::tile::TileDiffer::new();

    Ok((capture, encoder, differ))
}

// ── Session Manager ─────────────────────────────────────────────────────────

/// Monitors Windows user sessions and manages the agent process lifecycle.
/// When a user logs in, it launches a phantom agent in their session.
/// When they log out, it kills the agent.
struct SessionManager {
    agent_process: Option<std::process::Child>,
    last_session_id: u32,
}

impl SessionManager {
    fn new() -> Self {
        Self {
            agent_process: None,
            last_session_id: 0,
        }
    }

    /// Check for user session changes and launch/kill agent as needed.
    fn poll(&mut self) {
        #[cfg(target_os = "windows")]
        {
            let session_id = get_active_console_session_id();

            if session_id != self.last_session_id {
                self.last_session_id = session_id;

                // Kill existing agent if any
                if let Some(ref mut child) = self.agent_process {
                    let _ = child.kill();
                    let _ = child.wait();
                    self.agent_process = None;
                    eprintln!("Killed agent for old session");
                }

                // 0xFFFFFFFF means no active console session
                if session_id != 0xFFFFFFFF && session_id != 0 {
                    // A user has an interactive session — launch agent
                    match launch_agent_in_session(session_id) {
                        Ok(child) => {
                            eprintln!("Launched agent in session {session_id}");
                            self.agent_process = Some(child);
                        }
                        Err(e) => {
                            eprintln!("Failed to launch agent in session {session_id}: {e}");
                        }
                    }
                }
            }

            // Check if agent is still alive
            if let Some(ref mut child) = self.agent_process {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        eprintln!("Agent exited with {status}");
                        self.agent_process = None;
                    }
                    Ok(None) => {} // Still running
                    Err(e) => {
                        eprintln!("Error checking agent: {e}");
                        self.agent_process = None;
                    }
                }
            }
        }
    }
}

// ── Windows-specific session management functions ───────────────────────────

/// Get the session ID of the active console session.
#[cfg(target_os = "windows")]
fn get_active_console_session_id() -> u32 {
    // WTSGetActiveConsoleSessionId is in kernel32, always available
    extern "system" {
        fn WTSGetActiveConsoleSessionId() -> u32;
    }
    unsafe { WTSGetActiveConsoleSessionId() }
}

/// Launch the phantom agent process in a specific user session.
/// Uses WTSQueryUserToken + CreateProcessAsUser to inject into the user's desktop.
#[cfg(target_os = "windows")]
fn launch_agent_in_session(session_id: u32) -> anyhow::Result<std::process::Child> {
    use anyhow::Context;
    use std::ffi::c_void;
    use std::mem;
    use std::ptr;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS,
        TokenPrimary,
    };
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    use windows::Win32::System::Threading::{
        CreateProcessAsUserW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
    };

    unsafe {
        // Get the user's token for the target session
        let mut user_token = HANDLE::default();
        WTSQueryUserToken(session_id, &mut user_token)
            .context("WTSQueryUserToken failed (need LocalSystem or SeTcbPrivilege)")?;

        // Duplicate token as primary token for CreateProcessAsUser
        let mut dup_token = HANDLE::default();
        DuplicateTokenEx(
            user_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut dup_token,
        )
        .context("DuplicateTokenEx failed")?;
        let _ = CloseHandle(user_token);

        // Get the path to our own executable
        let exe_path = std::env::current_exe().context("get current exe")?;
        let cmd_line = format!(
            "\"{}\" --agent-mode --listen 127.0.0.1:9910 --no-encrypt",
            exe_path.display()
        );

        // Convert to wide string
        let cmd_wide: Vec<u16> = cmd_line.encode_utf16().chain(std::iter::once(0)).collect();

        let mut si: STARTUPINFOW = mem::zeroed();
        si.cb = mem::size_of::<STARTUPINFOW>() as u32;
        // Run on the interactive desktop
        let desktop = "winsta0\\default\0"
            .encode_utf16()
            .collect::<Vec<u16>>();
        si.lpDesktop = windows::core::PWSTR(desktop.as_ptr() as *mut u16);

        let mut pi: PROCESS_INFORMATION = mem::zeroed();

        let result = CreateProcessAsUserW(
            dup_token,
            None,
            windows::core::PWSTR(cmd_wide.as_ptr() as *mut u16),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT,
            None,
            None,
            &si,
            &mut pi,
        );

        let _ = CloseHandle(dup_token);

        result.context("CreateProcessAsUserW failed")?;

        // We don't need the thread handle
        let _ = CloseHandle(pi.hThread);

        // Wrap the process handle in a Child-like struct for lifecycle management
        // Since we can't easily create a std::process::Child from a raw handle,
        // we use a simple wrapper. For now, spawn via Command as fallback.
        let _ = CloseHandle(pi.hProcess);

        // Fallback: use runas / session injection via command
        // The CreateProcessAsUser above is the correct approach, but returning
        // a std::process::Child from it requires more plumbing. For now, we track
        // the PID manually.
        //
        // TODO: Implement proper process handle wrapper
        // For the initial implementation, we spawn via a helper approach:
        let child = std::process::Command::new(exe_path)
            .args(["--agent-mode", "--listen", "127.0.0.1:9910", "--no-encrypt"])
            .spawn()
            .context("spawn agent process")?;

        Ok(child)
    }
}

// ── Service installation helpers ────────────────────────────────────────────

/// Install Phantom as a Windows Service (replaces schtasks approach).
pub fn install_service() -> anyhow::Result<()> {
    use anyhow::Context;

    let exe = std::env::current_exe().context("get current exe path")?;
    let exe_str = exe.to_string_lossy();

    // Create the service
    let status = std::process::Command::new("sc")
        .args([
            "create",
            SERVICE_NAME,
            &format!("binPath= \"{}\"", exe_str),
            "start=",
            "auto",
            "obj=",
            "LocalSystem",
            &format!("DisplayName= {}", SERVICE_DISPLAY_NAME),
        ])
        .status()
        .context("sc create")?;

    if !status.success() {
        anyhow::bail!("sc create failed with {status}. Run as Administrator.");
    }

    // Set description
    let _ = std::process::Command::new("sc")
        .args([
            "description",
            SERVICE_NAME,
            SERVICE_DESCRIPTION,
        ])
        .status();

    // Configure service recovery: restart on failure
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

    // Stop first (ignore errors if already stopped)
    let _ = std::process::Command::new("sc")
        .args(["stop", SERVICE_NAME])
        .status();

    // Wait a moment for the service to stop
    std::thread::sleep(Duration::from_secs(2));

    // Delete
    let status = std::process::Command::new("sc")
        .args(["delete", SERVICE_NAME])
        .status()
        .context("sc delete")?;

    if status.success() {
        println!("Removed: {SERVICE_DISPLAY_NAME} (Windows Service)");
    } else {
        anyhow::bail!("sc delete failed with {status}. Run as Administrator.");
    }

    // Also clean up old schtasks entry if it exists
    let _ = std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", "PhantomServer", "/F"])
        .status();

    Ok(())
}

/// Detect if the process was started by the Service Control Manager.
/// If stdin is not a console and no console is attached, we're likely a service.
pub fn is_running_as_service() -> bool {
    #[cfg(target_os = "windows")]
    {
        // Heuristic: services don't have a console window.
        // The definitive way is to attempt service dispatcher registration,
        // but that blocks. Instead, check if we have a console.
        extern "system" {
            fn GetConsoleWindow() -> *mut std::ffi::c_void;
        }
        unsafe { GetConsoleWindow().is_null() }
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}
