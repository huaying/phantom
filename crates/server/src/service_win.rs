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
//!   console session and the currently-visible desktop (`Default` or
//!   `Winlogon`). The service relaunches the agent when Windows reports
//!   lock/unlock/logout transitions instead of expecting one process to cross
//!   secure desktop boundaries.
//! - No Session 0 capture: GDI/DXGI cannot capture cross-session desktops.
//!   The service waits for the agent to be ready before serving frames.

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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
use std::time::{Duration, Instant};
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

// Virtual Display Driver (VDD) — provides a virtual monitor on headless GPU servers
// so DXGI Desktop Duplication can capture from the NVIDIA GPU.
const VDD_HARDWARE_ID: &str = r"Root\MttVDD";
const VDD_CLASS_GUID: &str = "{4D36E968-E325-11CE-BFC1-08002BE10318}";
const VDD_DRIVER_URL: &str = "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/download/25.7.23/VirtualDisplayDriver-x86.Driver.Only.zip";
const NEFCON_URL: &str =
    "https://github.com/nefarius/nefcon/releases/download/v1.17.40/nefcon_v1.17.40.zip";

/// Entry point when invoked by the Windows Service Control Manager.
/// Call this from main() when `--service` flag is passed.
pub fn run_as_service() -> Result<(), windows_service::Error> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

// The service main function called by SCM via the dispatcher.
// windows-service requires this exact signature.
define_windows_service!(ffi_service_main, phantom_service_main);

fn phantom_service_main(arguments: Vec<OsString>) {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        svc_log(&format!("service panic: {info}"));
        previous_hook(info);
    }));

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_service(arguments)));
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            svc_log(&format!("Service failed: {e:#}"));
            tracing::error!("Service failed: {e}");
            std::process::exit(1);
        }
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                *s
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.as_str()
            } else {
                "unknown panic payload"
            };
            svc_log(&format!("Service panicked: {msg}"));
            tracing::error!("Service panicked: {msg}");
            // Do not swallow the panic and leave the service silently stopped.
            // Exiting non-zero lets SCM failure actions restart the service.
            std::process::exit(101);
        }
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
                    // A user logged in/out/locked/unlocked. Keep the active
                    // viewer socket alive; the service-mode relay will swap
                    // the user-session agent/IPC underneath it and wait for a
                    // fresh keyframe. Only real shutdown/new-client takeover
                    // should flip `cancel`.
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
    use crate::transport::tcp;
    use crate::transport::ws;
    use phantom_core::transport::{MessageReceiver, MessageSender};
    use std::sync::mpsc;

    type ConnectionPair = (Box<dyn MessageSender>, Box<dyn MessageReceiver>);
    // After the doorbell has pulled ClientHello off the wire, it attaches
    // any resolution hint (Some((w, h)) when client's viewport suggests a
    // specific VDD size; None for legacy / unhinted clients).
    type PendingSession = (
        Box<dyn MessageSender>,
        Box<dyn MessageReceiver>,
        Option<(u32, u32)>,
    );

    let listen_addr = "0.0.0.0:9900";
    let base_port: u16 = 9900;

    // Start TCP listener
    let tcp_listener = tcp::TcpServerTransport::bind(listen_addr)?;
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
    let ws_transport =
        ws::WebServerTransport::start(base_port + 1, base_port + 2, base_port + 3, auth_secret)?;
    let tx = conn_tx.clone();
    std::thread::Builder::new()
        .name("svc-web-accept".into())
        .spawn(move || loop {
            #[cfg(feature = "webrtc")]
            let accept_result = ws_transport.accept_any();
            #[cfg(not(feature = "webrtc"))]
            let accept_result = ws_transport.accept_ws();

            match accept_result {
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
    let pending: Arc<std::sync::Mutex<Option<PendingSession>>> =
        Arc::new(std::sync::Mutex::new(None));
    // cancel: shared with Stop handler (passed from run_service)
    let conn_rx = Arc::new(std::sync::Mutex::new(conn_rx));
    // Client-identity tracking for thrash prevention.
    //  - `current_client_id`: the id of whoever owns the active session (if any)
    //  - `ghost_ids`: ids we've recently kicked; auto-reconnect attempts
    //    from these ids get rejected immediately so they can't steal back
    //    the session from the new legitimate owner.
    // Using a small VecDeque capped at 16 entries — plenty for the "N
    // forgotten browser tabs" scenario, and we don't want this to grow
    // unbounded.
    let current_client_id: Arc<std::sync::Mutex<Option<[u8; 16]>>> =
        Arc::new(std::sync::Mutex::new(None));
    let ghost_ids: Arc<std::sync::Mutex<std::collections::VecDeque<[u8; 16]>>> =
        Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::with_capacity(crate::doorbell::GHOST_MAX),
        ));

    // Doorbell thread
    {
        let conn_rx = Arc::clone(&conn_rx);
        let pending = Arc::clone(&pending);
        let cancel = Arc::clone(&cancel);
        let current_client_id = Arc::clone(&current_client_id);
        let ghost_ids = Arc::clone(&ghost_ids);
        std::thread::Builder::new()
            .name("svc-doorbell".into())
            .spawn(move || loop {
                let pair = { conn_rx.lock().unwrap().recv() };
                match pair {
                    Ok((mut sender, mut receiver)) => {
                        // Expect ClientHello as the first message. Clients
                        // built after we added client-id tracking always send
                        // one; legacy clients don't, and we grant them a
                        // 500ms grace window before treating them as "no id".
                        let (id, resolution_hint): (Option<[u8; 16]>, Option<(u32, u32)>) =
                            match receiver.recv_msg_within(Duration::from_millis(500)) {
                                Ok(Some(phantom_core::protocol::Message::ClientHello {
                                    client_id,
                                    preferred_width,
                                    preferred_height,
                                })) => {
                                    let hint = if preferred_width > 0 && preferred_height > 0 {
                                        Some((preferred_width, preferred_height))
                                    } else {
                                        None
                                    };
                                    (Some(client_id), hint)
                                }
                                _ => (None, None),
                            };

                        // Decision logic shared with main.rs (see doorbell module).
                        let mut cur = current_client_id.lock().unwrap();
                        let mut ghosts = ghost_ids.lock().unwrap();
                        let decision = crate::doorbell::decide(id, &mut cur, &mut ghosts);
                        drop(cur);
                        drop(ghosts);

                        if matches!(decision, crate::doorbell::DoorbellDecision::Reject) {
                            tracing::info!("Doorbell: rejecting ghost client (already kicked)");
                            let _ = sender.send_msg(&phantom_core::protocol::Message::Disconnect {
                                reason: "ghost client rejected".to_string(),
                            });
                            drop(sender);
                            drop(receiver);
                            continue;
                        }

                        let had_existing = pending.lock().unwrap().is_some();
                        *pending.lock().unwrap() = Some((sender, receiver, resolution_hint));
                        if had_existing {
                            tracing::info!("New client arrived, replacing queued connection");
                        }
                        cancel.store(true, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            })?;
    }

    // Session-change watcher thread. The active viewer may be inside
    // `create_service_session()` for a long time, so the outer accept loop
    // cannot be the only place that notices Windows desktop/session drift.
    // The watcher only marks drift; the relay keeps the viewer connected and
    // swaps to a freshly launched agent/IPC in-place.
    {
        let shutdown = Arc::clone(&shutdown);
        let session_changed = Arc::clone(&session_changed);
        std::thread::Builder::new()
            .name("svc-session-watch".into())
            .spawn(move || {
                let mut last_seen = get_active_console_session_id();
                let mut last_desktop_kind = if is_valid_console_session_id(last_seen) {
                    Some(detect_agent_desktop_kind(last_seen))
                } else {
                    None
                };
                while !shutdown.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                    let cur = get_active_console_session_id();
                    let cur_desktop_kind = if is_valid_console_session_id(cur) {
                        Some(detect_agent_desktop_kind(cur))
                    } else {
                        None
                    };
                    if cur != last_seen || cur_desktop_kind != last_desktop_kind {
                        svc_log(&format!(
                            "Watcher: session/desktop changed {last_seen:?}/{last_desktop_kind:?} -> {cur:?}/{cur_desktop_kind:?}, refreshing agent"
                        ));
                        session_changed.store(true, Ordering::Relaxed);
                        last_seen = cur;
                        last_desktop_kind = cur_desktop_kind;
                    }
                }
            })?;
    }

    let mut last_active_desktop_probe: Option<(u32, Option<AgentDesktopKind>)> = None;
    let mut last_active_desktop_probe_at = Instant::now() - Duration::from_secs(1);

    while !shutdown.load(Ordering::Relaxed) {
        // Check for session changes (driven by SCM SESSION_CHANGE events).
        // Also poll periodically because SCM events are not 100% reliable:
        // - User-switch ("Switch user") may not fire CONSOLE_DISCONNECT reliably
        // - Event may fire before WTSGetActiveConsoleSessionId reflects new state
        // - Agent may be alive + IPC connected but in a now-Disconnected session
        //   (pipe stays open across sessions, so ipc_alive is a lie here)
        //
        // So check four conditions that require update():
        //  1. Explicit SCM event fired
        //  2. Missing agent/IPC (first-launch, crash, or we killed it)
        //  3. Active console session ID changed since last update
        //  4. Active desktop kind changed (Default <-> Winlogon)
        let active_session = get_active_console_session_id();
        let active_desktop_kind = if is_valid_console_session_id(active_session) {
            let should_probe = last_active_desktop_probe_at.elapsed() >= Duration::from_secs(1)
                || last_active_desktop_probe.is_none_or(|(sid, _)| sid != active_session);
            if should_probe {
                let kind = Some(detect_agent_desktop_kind(active_session));
                last_active_desktop_probe = Some((active_session, kind));
                last_active_desktop_probe_at = Instant::now();
                kind
            } else {
                last_active_desktop_probe
                    .and_then(|(sid, kind)| (sid == active_session).then_some(kind))
                    .flatten()
            }
        } else {
            last_active_desktop_probe = None;
            None
        };
        let session_drift = is_valid_console_session_id(active_session)
            && active_session != session_mgr.current_session_id;
        let desktop_drift = is_valid_console_session_id(active_session)
            && session_mgr.current_session_id == active_session
            && session_mgr.agent.is_some()
            && session_mgr.current_desktop_kind != active_desktop_kind;
        if session_changed.swap(false, Ordering::Relaxed)
            || session_mgr.agent.is_none()
            || session_mgr.ipc().is_none()
            || session_drift
            || desktop_drift
        {
            if session_drift {
                svc_log(&format!(
                    "Session drift detected: active={active_session} current={}",
                    session_mgr.current_session_id
                ));
            }
            if desktop_drift {
                svc_log(&format!(
                    "Desktop drift detected: active={active_desktop_kind:?} current={:?}",
                    session_mgr.current_desktop_kind
                ));
            }
            session_mgr.update();
        }
        // Also check if agent died unexpectedly
        session_mgr.check_agent_health();

        // Check for pending connection
        let conn = pending.lock().unwrap().take();
        if let Some((sender, receiver, resolution_hint)) = conn {
            cancel.store(false, Ordering::Relaxed);
            let session_cancel = Arc::clone(&cancel);

            // In service mode, relay agent frames via IPC (DXGI/GDI from user session).
            // If agent is not connected yet, reject the client.
            match create_service_session(
                &mut session_mgr,
                sender,
                receiver,
                session_cancel,
                Arc::clone(&session_changed),
                resolution_hint,
            ) {
                Ok(result) => {
                    // session end already logged by make_session_result;
                    // result.session_id / result.reason available if needed.
                    let _ = result;
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
    session_changed: Arc<AtomicBool>,
    resolution_hint: Option<(u32, u32)>,
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
        let resolution_hint = if session_mgr.current_desktop_kind
            == Some(AgentDesktopKind::Winlogon)
        {
            if let Some((hw, hh)) = resolution_hint {
                svc_log(&format!(
                        "create_service_session: ignoring resolution hint {hw}x{hh} on Winlogon desktop"
                    ));
            }
            None
        } else if windows_tier1_fixed_resolution_enabled() {
            if let Some((hw, hh)) = resolution_hint {
                svc_log(&format!(
                    "create_service_session: ignoring resolution hint {hw}x{hh} while Windows Tier 1 fixed-resolution mode is enabled"
                ));
            }
            None
        } else {
            resolution_hint
        };
        let ipc = session_mgr.ipc.as_ref().unwrap();
        let startup_viewer_guard = IpcViewerActiveGuard::new(ipc);
        let controls = DynamicAgentControls::default();
        sync_controls_to_current_ipc(session_mgr, &controls);

        // Drain queued frames from the previous client/mode before applying
        // this client's viewport hint; otherwise first-frame selection can see
        // a burst of stale old-resolution frames and start the session blurry.
        // Keep the latest matching keyframe, though: the agent may have already
        // prewarmed Tier 1 while idle, and that is a valid startup frame.
        let cached_keyframe = ipc.last_keyframe();
        let drained_frames = ipc.recv_encoded_frames();
        let drained = drained_frames.len();
        let mut prewarmed_startup_frame = None;
        let mut prewarmed_fallback_keyframe = None;
        if let Some(ef) = cached_keyframe {
            let matches_hint =
                resolution_hint.is_none_or(|(hw, hh)| ef.width == hw && ef.height == hh);
            if matches_hint {
                prewarmed_startup_frame = Some(ef);
            } else {
                prewarmed_fallback_keyframe = Some(ef);
            }
        }
        for ef in drained_frames {
            let matches_hint =
                resolution_hint.is_none_or(|(hw, hh)| ef.width == hw && ef.height == hh);
            if ef.encoded.is_keyframe && matches_hint {
                prewarmed_startup_frame = Some(ef);
            } else if ef.encoded.is_keyframe {
                prewarmed_fallback_keyframe = Some(ef);
            }
        }
        if drained > 0 {
            svc_log(&format!(
                "create_service_session: drained {drained} queued IPC frames before startup"
            ));
        }
        if let Some(ref ef) = prewarmed_startup_frame {
            svc_log(&format!(
                "create_service_session: found prewarmed startup keyframe {}x{} {} bytes",
                ef.width,
                ef.height,
                ef.encoded.data.len()
            ));
        } else if let Some(ref ef) = prewarmed_fallback_keyframe {
            svc_log(&format!(
                "create_service_session: found prewarmed fallback keyframe {}x{} {} bytes",
                ef.width,
                ef.height,
                ef.encoded.data.len()
            ));
        }

        // If the client hinted a preferred resolution, apply it now so the
        // first frame we wait for below is already at that size. The agent's
        // capture loop polls the resolution arc each iteration; when it sees
        // a change it drops the pipeline, reinits at the new mode, and
        // resumes producing frames. We then discard any stale frames still
        // in the IPC pipe that don't match the hint.
        if let Some((hw, hh)) = resolution_hint {
            controls.request_resolution_change(hw, hh);
            svc_log(&format!(
                "create_service_session: resolution hint {hw}x{hh} applied"
            ));
        }

        // Request keyframe from agent — triggers DXGI capture reset so the
        // agent produces a frame even on a static desktop.
        let _ = ipc.request_keyframe();

        // Wait for a decodable startup keyframe from the agent. Starting a web
        // session from a delta frame leaves the browser black until another
        // keyframe happens to arrive, which is exactly the failure mode users
        // perceive as "connected but screen is down".
        let mut attempts = 0;
        let mut last_keyframe_nudge = Instant::now();
        svc_log("Waiting for first keyframe from agent...");
        let mut fallback_keyframe: Option<crate::ipc_pipe::IpcEncodedFrame> =
            prewarmed_fallback_keyframe;
        let mut fallback_frame_size: Option<(u32, u32)> =
            fallback_keyframe.as_ref().map(|ef| (ef.width, ef.height));
        let mut fallback_since: Option<Instant> =
            fallback_keyframe.as_ref().map(|_| Instant::now());
        let mut suspicious_keyframe_since: Option<Instant> = None;
        let mut stale_frame_logs = 0u32;
        let startup_frame = if let Some(ef) = prewarmed_startup_frame {
            svc_log(&format!(
                "Using prewarmed startup keyframe: {}x{} {} bytes",
                ef.width,
                ef.height,
                ef.encoded.data.len()
            ));
            ef
        } else {
            'wait: loop {
                if cancel.load(Ordering::Relaxed) {
                    anyhow::bail!("service session cancelled while waiting for startup keyframe");
                }
                for ef in ipc.recv_encoded_frames() {
                    if let Some((hw, hh)) = resolution_hint {
                        if ef.width != hw || ef.height != hh {
                            stale_frame_logs += 1;
                            if stale_frame_logs <= 5 || stale_frame_logs.is_multiple_of(30) {
                                svc_log(&format!(
                                    "Discarding non-hinted frame {}x{} (waiting for {}x{}, count={})",
                                    ef.width, ef.height, hw, hh, stale_frame_logs
                                ));
                            }
                            fallback_frame_size = Some((ef.width, ef.height));
                            fallback_since.get_or_insert_with(Instant::now);
                            if ef.encoded.is_keyframe {
                                fallback_keyframe = Some(ef);
                            }
                            continue;
                        }
                    }
                    if !ef.encoded.is_keyframe {
                        stale_frame_logs += 1;
                        if stale_frame_logs <= 5 || stale_frame_logs.is_multiple_of(30) {
                            svc_log(&format!(
                            "Skipping startup delta frame {}x{} (waiting for keyframe, count={})",
                            ef.width, ef.height, stale_frame_logs
                        ));
                        }
                        continue;
                    }
                    if is_suspicious_transition_keyframe(&ef) {
                        stale_frame_logs += 1;
                        let suspicious_since =
                            suspicious_keyframe_since.get_or_insert_with(Instant::now);
                        if suspicious_since.elapsed() < Duration::from_millis(750) {
                            if stale_frame_logs <= 5 || stale_frame_logs.is_multiple_of(30) {
                                svc_log(&format!(
                                    "Skipping suspicious startup keyframe {}x{} {} bytes (count={})",
                                    ef.width,
                                    ef.height,
                                    ef.encoded.data.len(),
                                    stale_frame_logs
                                ));
                            }
                            fallback_frame_size = Some((ef.width, ef.height));
                            fallback_since.get_or_insert_with(Instant::now);
                            let _ = ipc.request_keyframe();
                            continue;
                        }
                        if stale_frame_logs <= 5 || stale_frame_logs.is_multiple_of(30) {
                            svc_log(&format!(
                                "Accepting suspicious startup keyframe after transition wait {}x{} {} bytes (count={})",
                                ef.width,
                                ef.height,
                                ef.encoded.data.len(),
                                stale_frame_logs
                            ));
                        }
                    }
                    svc_log(&format!(
                        "Got startup keyframe: {}x{} {} bytes",
                        ef.width,
                        ef.height,
                        ef.encoded.data.len()
                    ));
                    tracing::info!(
                        width = ef.width,
                        height = ef.height,
                        bytes = ef.encoded.data.len(),
                        keyframe = ef.encoded.is_keyframe,
                        "Got encoded frame from agent"
                    );
                    break 'wait ef;
                }
                if let Some(ref ef) = fallback_keyframe {
                    if fallback_since
                        .is_some_and(|since| since.elapsed() > Duration::from_millis(750))
                    {
                        svc_log(&format!(
                        "No keyframe matched resolution hint quickly; accepting live fallback keyframe {}x{}",
                        ef.width, ef.height
                    ));
                        break 'wait fallback_keyframe.take().expect("checked Some");
                    }
                }
                attempts += 1;
                if attempts % 10 == 0 {
                    tracing::debug!("Still waiting for agent frame... attempt {attempts}/100");
                }
                // Keep nudging the agent while waiting for the very first frame.
                // Resolution switches can consume the initial keyframe request;
                // repeated nudges avoid timing out into a black session.
                if last_keyframe_nudge.elapsed() > Duration::from_millis(500) {
                    let _ = ipc.request_keyframe();
                    last_keyframe_nudge = Instant::now();
                }
                // Windows session transitions can take several seconds before
                // Desktop Duplication or the same-desktop GDI bootstrap returns
                // a real keyframe. Do not fail early into a black 300x150 web
                // canvas; keep the viewer attached and let the agent recover.
                let cap = 1000;
                if attempts > cap {
                    if let Some(ef) = fallback_keyframe {
                        svc_log(&format!(
                            "No keyframe matched resolution hint in time; falling back to {}x{}",
                            ef.width, ef.height
                        ));
                        break 'wait ef;
                    }
                    if let Some((hw, hh)) = resolution_hint {
                        svc_log(&format!(
                        "No real startup keyframe from agent within timeout for resolution hint {}x{}",
                        hw, hh
                    ));
                    }
                    if let Some((fw, fh)) = fallback_frame_size {
                        svc_log(&format!(
                        "Only delta fallback frames arrived (latest {}x{}); refusing black startup",
                        fw, fh
                    ));
                    }
                    svc_log("No matching startup keyframe from agent within timeout");
                    anyhow::bail!("agent connected but not producing startup keyframe in time");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let width = startup_frame.width;
        let height = startup_frame.height;
        let mut viewer_attached_to = None;
        attach_viewer_to_current_ipc(session_mgr, &mut viewer_attached_to);
        drop(startup_viewer_guard);

        let mut runner = crate::session::SessionRunner::new(
            sender,
            receiver,
            width,
            height,
            frame_interval,
            Arc::clone(&cancel),
            phantom_core::encode::VideoCodec::H264,
            false,
            false,
            false,
        )?;
        let session_id = runner.session_id.clone();
        runner.input_forwarder =
            Some(Box::new(controls.clone()) as Box<dyn crate::session::InputForwarder>);
        {
            let controls = controls.clone();
            runner.resolution_change_fn = Some(Box::new(move |w: u32, h: u32| {
                controls.request_resolution_change(w, h);
            }));
        }
        {
            let controls = controls.clone();
            runner.paste_fn = Some(Box::new(move |text: &str| {
                controls.request_paste(text);
            }));
        }

        let result = run_dynamic_ipc_relay(
            session_mgr,
            &mut runner,
            controls,
            session_changed,
            &mut viewer_attached_to,
            startup_frame,
        );
        release_viewer_from_current_ipc(session_mgr, &mut viewer_attached_to);
        Ok(crate::session::make_session_result(
            result,
            session_id,
            cancel.load(Ordering::Relaxed),
        ))
    }

    #[cfg(not(target_os = "windows"))]
    unreachable!()
}

#[cfg(target_os = "windows")]
fn windows_tier1_fixed_resolution_enabled() -> bool {
    !matches!(
        std::env::var("PHANTOM_WINDOWS_TIER1_ADAPTIVE")
            .ok()
            .as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

#[cfg(target_os = "windows")]
type ViewerAttachKey = (u32, Option<AgentDesktopKind>);

#[cfg(target_os = "windows")]
struct IpcViewerActiveGuard<'a> {
    ipc: &'a crate::ipc_pipe::IpcServer,
}

#[cfg(target_os = "windows")]
impl<'a> IpcViewerActiveGuard<'a> {
    fn new(ipc: &'a crate::ipc_pipe::IpcServer) -> Self {
        ipc.acquire_viewer();
        Self { ipc }
    }
}

#[cfg(target_os = "windows")]
impl Drop for IpcViewerActiveGuard<'_> {
    fn drop(&mut self) {
        self.ipc.release_viewer();
    }
}

#[cfg(target_os = "windows")]
#[derive(Clone, Default)]
struct DynamicAgentControls {
    input_tx: Arc<Mutex<Option<std::sync::mpsc::Sender<phantom_core::input::InputEvent>>>>,
    resolution_arc: Arc<Mutex<Option<Arc<Mutex<Option<(u32, u32)>>>>>>,
    paste_arc: Arc<Mutex<Option<Arc<Mutex<Option<String>>>>>>,
    desired_resolution: Arc<Mutex<Option<(u32, u32)>>>,
}

#[cfg(target_os = "windows")]
impl DynamicAgentControls {
    fn update_from_ipc(
        &self,
        ipc: Option<&crate::ipc_pipe::IpcServer>,
        allow_resolution_changes: bool,
    ) {
        let input_tx = ipc.and_then(|ipc| ipc.input_sender());
        let resolution_arc = ipc
            .filter(|_| allow_resolution_changes)
            .map(|ipc| ipc.resolution_change_arc());
        let paste_arc = ipc.map(|ipc| ipc.paste_arc());

        *self.input_tx.lock().unwrap_or_else(|e| e.into_inner()) = input_tx;
        *self
            .resolution_arc
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = resolution_arc.clone();
        *self.paste_arc.lock().unwrap_or_else(|e| e.into_inner()) = paste_arc;

        if let (Some(arc), Some((w, h))) = (
            resolution_arc,
            *self
                .desired_resolution
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        ) {
            *arc.lock().unwrap_or_else(|e| e.into_inner()) = Some((w, h));
        }
    }

    fn request_resolution_change(&self, width: u32, height: u32) {
        *self
            .desired_resolution
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((width, height));
        if let Some(arc) = self
            .resolution_arc
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .cloned()
        {
            *arc.lock().unwrap_or_else(|e| e.into_inner()) = Some((width, height));
        }
    }

    fn request_paste(&self, text: &str) {
        if let Some(arc) = self
            .paste_arc
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .cloned()
        {
            *arc.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.to_string());
        }
    }
}

#[cfg(target_os = "windows")]
impl crate::session::InputForwarder for DynamicAgentControls {
    fn forward_input(&self, event: &phantom_core::input::InputEvent) -> anyhow::Result<()> {
        let tx = self
            .input_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(tx) = tx {
            tx.send(event.clone())
                .map_err(|e| anyhow::anyhow!("IPC input forward failed: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn sync_controls_to_current_ipc(session_mgr: &SessionManager, controls: &DynamicAgentControls) {
    let allow_resolution_changes =
        session_mgr.current_desktop_kind != Some(AgentDesktopKind::Winlogon);
    controls.update_from_ipc(session_mgr.ipc(), allow_resolution_changes);
}

#[cfg(target_os = "windows")]
fn attach_viewer_to_current_ipc(
    session_mgr: &SessionManager,
    attached_to: &mut Option<ViewerAttachKey>,
) {
    let key = (
        session_mgr.current_session_id,
        session_mgr.current_desktop_kind,
    );
    if attached_to.as_ref() == Some(&key) {
        return;
    }
    if let Some(ipc) = session_mgr.ipc() {
        ipc.acquire_viewer();
        *attached_to = Some(key);
    }
}

#[cfg(target_os = "windows")]
fn release_viewer_from_current_ipc(
    session_mgr: &SessionManager,
    attached_to: &mut Option<ViewerAttachKey>,
) {
    let key = (
        session_mgr.current_session_id,
        session_mgr.current_desktop_kind,
    );
    if attached_to.as_ref() == Some(&key) {
        if let Some(ipc) = session_mgr.ipc() {
            ipc.release_viewer();
        }
    }
    *attached_to = None;
}

#[cfg(target_os = "windows")]
fn service_ipc_refresh_needed(session_mgr: &SessionManager, session_changed: &AtomicBool) -> bool {
    let active_session = get_active_console_session_id();
    let active_desktop_kind = if is_valid_console_session_id(active_session) {
        Some(detect_agent_desktop_kind(active_session))
    } else {
        None
    };
    let session_drift = is_valid_console_session_id(active_session)
        && active_session != session_mgr.current_session_id;
    let desktop_drift = is_valid_console_session_id(active_session)
        && session_mgr.current_session_id == active_session
        && session_mgr.current_desktop_kind != active_desktop_kind;

    if session_drift {
        svc_log(&format!(
            "Relay session drift: active={active_session} current={}",
            session_mgr.current_session_id
        ));
    }
    if desktop_drift {
        svc_log(&format!(
            "Relay desktop drift: active={active_desktop_kind:?} current={:?}",
            session_mgr.current_desktop_kind
        ));
    }

    session_changed.swap(false, Ordering::Relaxed)
        || session_mgr.agent.is_none()
        || session_mgr.ipc().is_none()
        || session_drift
        || desktop_drift
}

#[cfg(target_os = "windows")]
fn run_dynamic_ipc_relay(
    session_mgr: &mut SessionManager,
    runner: &mut crate::session::SessionRunner,
    controls: DynamicAgentControls,
    session_changed: Arc<AtomicBool>,
    viewer_attached_to: &mut Option<ViewerAttachKey>,
    startup_frame: crate::ipc_pipe::IpcEncodedFrame,
) -> anyhow::Result<Vec<u8>> {
    if startup_frame.width != runner.current_width || startup_frame.height != runner.current_height
    {
        tracing::info!(
            old_w = runner.current_width,
            old_h = runner.current_height,
            new_w = startup_frame.width,
            new_h = startup_frame.height,
            "Startup frame resolution changed"
        );
        runner.current_width = startup_frame.width;
        runner.current_height = startup_frame.height;
    }
    runner.send_video_frame(startup_frame.encoded, None)?;

    let mut wait_for_live_keyframe = true;
    let mut skipped_suspicious_transition_keyframe = false;
    let mut last_keyframe_nudge = Instant::now() - Duration::from_secs(1);
    let mut last_agent_refresh = Instant::now() - Duration::from_secs(1);
    let mut last_no_ipc_log = Instant::now() - Duration::from_secs(5);

    if let Some(ipc) = session_mgr.ipc() {
        let _ = ipc.request_keyframe();
    }

    loop {
        runner.check_cancelled()?;
        let loop_start = Instant::now();

        session_mgr.check_agent_health();
        if last_agent_refresh.elapsed() >= Duration::from_millis(250)
            && service_ipc_refresh_needed(session_mgr, &session_changed)
        {
            last_agent_refresh = Instant::now();
            svc_log("Relay: refreshing Windows agent/IPC without dropping viewer");
            release_viewer_from_current_ipc(session_mgr, viewer_attached_to);
            session_mgr.update();
            sync_controls_to_current_ipc(session_mgr, &controls);
            attach_viewer_to_current_ipc(session_mgr, viewer_attached_to);
            wait_for_live_keyframe = true;
            skipped_suspicious_transition_keyframe = false;
            last_keyframe_nudge = Instant::now() - Duration::from_secs(1);
            if let Some(ipc) = session_mgr.ipc() {
                let _ = ipc.request_keyframe();
            }
        }

        runner.pump_events()?;
        runner.poll_clipboard()?;
        runner.drain_audio()?;

        if let Some(ipc) = session_mgr.ipc() {
            let should_request_keyframe = wait_for_live_keyframe || runner.needs_keyframe();
            if should_request_keyframe && last_keyframe_nudge.elapsed() > Duration::from_millis(500)
            {
                let _ = ipc.request_keyframe();
                last_keyframe_nudge = Instant::now();
            }

            for ipc_frame in ipc.recv_encoded_frames() {
                if wait_for_live_keyframe {
                    if ipc_frame.encoded.is_keyframe {
                        if !skipped_suspicious_transition_keyframe
                            && is_suspicious_transition_keyframe(&ipc_frame)
                        {
                            skipped_suspicious_transition_keyframe = true;
                            svc_log(&format!(
                                "Relay: skipped suspicious tiny transition keyframe {}x{} {} bytes",
                                ipc_frame.width,
                                ipc_frame.height,
                                ipc_frame.encoded.data.len()
                            ));
                            let _ = ipc.request_keyframe();
                            continue;
                        }
                        wait_for_live_keyframe = false;
                        svc_log(&format!(
                            "Relay: forwarding first live keyframe {}x{} after agent refresh",
                            ipc_frame.width, ipc_frame.height
                        ));
                    } else {
                        continue;
                    }
                }

                if ipc_frame.width != runner.current_width
                    || ipc_frame.height != runner.current_height
                {
                    tracing::info!(
                        old_w = runner.current_width,
                        old_h = runner.current_height,
                        new_w = ipc_frame.width,
                        new_h = ipc_frame.height,
                        "Frame resolution changed"
                    );
                    runner.current_width = ipc_frame.width;
                    runner.current_height = ipc_frame.height;
                }
                runner.send_video_frame(ipc_frame.encoded, None)?;
            }

            if let Some(text) = ipc.recv_clipboard() {
                let _ = runner
                    .sender
                    .send_msg(&phantom_core::protocol::Message::ClipboardSync(text));
            }
        } else if last_no_ipc_log.elapsed() >= Duration::from_secs(2) {
            svc_log("Relay: waiting for Windows agent IPC while keeping viewer connected");
            last_no_ipc_log = Instant::now();
        }

        runner.drain_file_transfers()?;
        runner.log_stats("stats-ipc");
        runner.keepalive_tick()?;
        runner.frame_pace(loop_start)?;
    }
}

#[cfg(target_os = "windows")]
fn is_suspicious_transition_keyframe(frame: &crate::ipc_pipe::IpcEncodedFrame) -> bool {
    let pixels = (frame.width as usize).saturating_mul(frame.height as usize);
    let min_expected_bytes = (pixels / 250).clamp(4096, 32 * 1024);
    frame.encoded.data.len() < min_expected_bytes
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
    fn wait(&self, timeout: Duration) -> bool {
        if self.handle.is_invalid() || self.pid == 0 {
            return false;
        }

        unsafe {
            use windows::Win32::Foundation::WAIT_OBJECT_0;
            use windows::Win32::System::Threading::WaitForSingleObject;

            let wait_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
            WaitForSingleObject(self.handle, wait_ms) == WAIT_OBJECT_0
        }
    }

    /// Terminate the process.
    fn terminate_and_wait(&self, timeout: Duration) -> bool {
        if self.handle.is_invalid() || self.pid == 0 {
            svc_log(&format!(
                "Agent PID={} has invalid handle; cannot terminate by handle",
                self.pid
            ));
            return false;
        }

        unsafe {
            use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
            use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};

            match TerminateProcess(self.handle, 1) {
                Ok(()) => {}
                Err(e) => {
                    svc_log(&format!(
                        "TerminateProcess(agent PID={}) failed: {e:#}",
                        self.pid
                    ));
                    return false;
                }
            }

            let wait_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
            match WaitForSingleObject(self.handle, wait_ms) {
                WAIT_OBJECT_0 => true,
                WAIT_TIMEOUT => {
                    svc_log(&format!(
                        "Agent PID={} did not exit within {}ms after terminate",
                        self.pid, wait_ms
                    ));
                    false
                }
                other => {
                    svc_log(&format!(
                        "WaitForSingleObject(agent PID={}) returned {:?}",
                        self.pid, other
                    ));
                    false
                }
            }
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

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentDesktopKind {
    Default,
    Winlogon,
}

#[cfg(target_os = "windows")]
const DESKTOP_KIND_STABLE_FOR: Duration = Duration::from_millis(1000);

#[cfg(target_os = "windows")]
#[derive(Debug, Default)]
struct DesktopDetectDebounce {
    last_confirmed_session: Option<u32>,
    last_confirmed_kind: Option<AgentDesktopKind>,
    candidate_session: Option<u32>,
    candidate_kind: Option<AgentDesktopKind>,
    candidate_since: Option<Instant>,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug)]
struct DesktopKindObservation {
    kind: AgentDesktopKind,
    force: bool,
}

#[cfg(target_os = "windows")]
static DESKTOP_DETECT_DEBOUNCE: OnceLock<Mutex<DesktopDetectDebounce>> = OnceLock::new();

/// Monitors Windows user sessions and manages the agent process lifecycle.
/// When a user logs in, it launches a phantom agent in their session
/// and establishes an IPC pipe for frame/input proxying.
struct SessionManager {
    #[cfg(target_os = "windows")]
    agent: Option<WinProcessHandle>,
    #[cfg(target_os = "windows")]
    current_session_id: u32,
    #[cfg(target_os = "windows")]
    current_desktop_kind: Option<AgentDesktopKind>,
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
            current_desktop_kind: None,
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
            let mut session_id = get_active_console_session_id();
            let desired_desktop_kind = if is_valid_console_session_id(session_id) {
                Some(detect_agent_desktop_kind(session_id))
            } else {
                None
            };
            svc_log(&format!(
                "update: current={}/{:?} detected={}/{:?} agent={} ipc={}",
                self.current_session_id,
                self.current_desktop_kind,
                session_id,
                desired_desktop_kind,
                self.agent.is_some(),
                self.ipc.is_some()
            ));

            // Check IPC health — if IO threads died, clean up so we relaunch.
            let ipc_alive = self.ipc.as_ref().is_some_and(|ipc| ipc.is_connected());
            if self.ipc.is_some() && !ipc_alive {
                svc_log("IPC IO threads dead — cleaning up for relaunch");
                if let Some(mut ipc) = self.ipc.take() {
                    ipc.disconnect();
                }
            }

            if session_id == self.current_session_id
                && desired_desktop_kind == self.current_desktop_kind
                && self.agent.is_some()
                && ipc_alive
            {
                return; // Session unchanged and agent is healthy — nothing to do
            }

            // No valid session yet (boot race: winlogon hasn't created Session 1)
            if !is_valid_console_session_id(session_id) {
                if self.agent.is_some() || self.ipc.is_some() {
                    svc_log(&format!(
                        "No valid console session ({session_id}); killing current agent"
                    ));
                    self.kill_agent();
                }
                self.current_session_id = session_id;
                self.current_desktop_kind = None;
                return;
            }

            let mut desired_desktop_kind =
                desired_desktop_kind.expect("valid session has desktop kind");

            // Session or desktop changed — kill old agent before launching new one.
            if session_id != self.current_session_id
                || self.current_desktop_kind != Some(desired_desktop_kind)
            {
                svc_log(&format!(
                    "Session/desktop changed: {}/{:?} -> {}/{:?}",
                    self.current_session_id,
                    self.current_desktop_kind,
                    session_id,
                    desired_desktop_kind
                ));
                self.current_session_id = session_id;
                self.current_desktop_kind = Some(desired_desktop_kind);
                self.kill_agent();

                // Stopping the old agent can take several seconds. During
                // Windows login/logout transitions the visible desktop often
                // changes again in that window (Winlogon spinner -> Default,
                // or Default -> Winlogon). Re-read the target before creating
                // the new process so we never launch an agent for a stale
                // desktop decision.
                let refreshed_session_id = get_active_console_session_id();
                if !is_valid_console_session_id(refreshed_session_id) {
                    svc_log(&format!(
                        "Console session became invalid ({refreshed_session_id}) while stopping agent; deferring relaunch"
                    ));
                    self.current_session_id = refreshed_session_id;
                    self.current_desktop_kind = None;
                    return;
                }
                let refreshed_desktop_kind = detect_agent_desktop_kind(refreshed_session_id);
                if refreshed_session_id != session_id
                    || refreshed_desktop_kind != desired_desktop_kind
                {
                    svc_log(&format!(
                        "Retargeting agent launch after stop: {session_id}/{desired_desktop_kind:?} -> {refreshed_session_id}/{refreshed_desktop_kind:?}"
                    ));
                    session_id = refreshed_session_id;
                    desired_desktop_kind = refreshed_desktop_kind;
                    self.current_session_id = session_id;
                    self.current_desktop_kind = Some(desired_desktop_kind);
                }
            }

            // Already have a working agent — nothing to do
            if self.agent.is_some() && ipc_alive {
                return;
            }

            // Need to launch agent (first time, or after crash/session change)
            if self.agent.is_some() {
                // Agent exists but IPC is broken — kill and relaunch
                svc_log("Agent exists but IPC disconnected, relaunching");
                self.kill_agent();
            }
            kill_lingering_agents_for_other_sessions(session_id);

            {
                svc_log(&format!("Creating IPC pipe for session {session_id}"));
                match crate::ipc_pipe::IpcServer::new(session_id) {
                    Ok(mut ipc_server) => {
                        svc_log(&format!(
                            "IPC pipe created, launching {desired_desktop_kind:?} agent"
                        ));
                        match launch_agent_in_session(session_id, desired_desktop_kind) {
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
                        match launch_agent_in_session(session_id, desired_desktop_kind) {
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
                tracing::info!(pid = agent.pid, "Stopping agent");
                if agent.wait(Duration::from_secs(3)) {
                    svc_log(&format!(
                        "Agent PID={} exited after IPC shutdown",
                        agent.pid
                    ));
                } else {
                    svc_log(&format!(
                        "Agent PID={} did not exit after IPC shutdown; terminating",
                        agent.pid
                    ));
                    if !agent.terminate_and_wait(Duration::from_secs(3)) {
                        force_kill_process(agent.pid);
                    }
                }
                // Handle is closed on drop.
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

#[cfg(target_os = "windows")]
fn is_valid_console_session_id(session_id: u32) -> bool {
    session_id != 0 && session_id != 0xFFFFFFFF
}

#[cfg(target_os = "windows")]
fn detect_agent_desktop_kind(session_id: u32) -> AgentDesktopKind {
    debounce_desktop_kind(session_id, detect_agent_desktop_kind_raw(session_id))
}

#[cfg(target_os = "windows")]
fn detect_agent_desktop_kind_raw(session_id: u32) -> DesktopKindObservation {
    // WTS lock state and explorer.exe can lead the real desktop during login:
    // Windows may report "unlocked" and create explorer while Winlogon is still
    // the visible/input desktop showing the login spinner. Follow the actual
    // input desktop first, like VNC-style Windows services do, but force only
    // hard state changes (locked/no shell) to avoid login-transition flapping.
    let locked = session_is_locked(session_id).unwrap_or(false);
    let shell_ready = find_process_in_session("explorer.exe", session_id).is_some();

    if let Some(kind) = active_input_desktop_kind() {
        return match kind {
            AgentDesktopKind::Default => {
                if shell_ready && !locked {
                    DesktopKindObservation {
                        kind: AgentDesktopKind::Default,
                        // This is the same signal open-source Windows hosts
                        // ultimately trust: the input desktop is Default and
                        // the user's shell exists. Do not keep showing
                        // Winlogon for the generic debounce window.
                        force: true,
                    }
                } else {
                    DesktopKindObservation {
                        kind: AgentDesktopKind::Winlogon,
                        force: true,
                    }
                }
            }
            AgentDesktopKind::Winlogon => DesktopKindObservation {
                kind: AgentDesktopKind::Winlogon,
                force: locked || !shell_ready,
            },
        };
    }

    if !shell_ready || locked {
        DesktopKindObservation {
            kind: AgentDesktopKind::Winlogon,
            force: true,
        }
    } else {
        DesktopKindObservation {
            kind: AgentDesktopKind::Default,
            force: false,
        }
    }
}

#[cfg(target_os = "windows")]
fn debounce_desktop_kind(session_id: u32, observed: DesktopKindObservation) -> AgentDesktopKind {
    let state =
        DESKTOP_DETECT_DEBOUNCE.get_or_init(|| Mutex::new(DesktopDetectDebounce::default()));
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());

    if state.last_confirmed_session != Some(session_id) || state.last_confirmed_kind.is_none() {
        state.last_confirmed_session = Some(session_id);
        state.last_confirmed_kind = Some(observed.kind);
        state.candidate_session = None;
        state.candidate_kind = None;
        state.candidate_since = None;
        return observed.kind;
    }

    if observed.force || state.last_confirmed_kind == Some(observed.kind) {
        state.last_confirmed_session = Some(session_id);
        state.last_confirmed_kind = Some(observed.kind);
        state.candidate_session = None;
        state.candidate_kind = None;
        state.candidate_since = None;
        return observed.kind;
    }

    let now = Instant::now();
    match (
        state.candidate_session,
        state.candidate_kind,
        state.candidate_since,
    ) {
        (Some(candidate_session), Some(candidate_kind), Some(since))
            if candidate_session == session_id && candidate_kind == observed.kind =>
        {
            if now.duration_since(since) >= DESKTOP_KIND_STABLE_FOR {
                let previous_kind = state.last_confirmed_kind;
                state.last_confirmed_session = Some(session_id);
                state.last_confirmed_kind = Some(observed.kind);
                state.candidate_session = None;
                state.candidate_kind = None;
                state.candidate_since = None;
                svc_log(&format!(
                    "{:?} desktop stable for {}ms; switching from {:?}",
                    observed.kind,
                    DESKTOP_KIND_STABLE_FOR.as_millis(),
                    previous_kind
                ));
                observed.kind
            } else {
                state.last_confirmed_kind.unwrap_or(observed.kind)
            }
        }
        _ => {
            state.candidate_session = Some(session_id);
            state.candidate_kind = Some(observed.kind);
            state.candidate_since = Some(now);
            svc_log(&format!(
                "{:?} desktop candidate detected after {:?}; waiting {}ms for stability",
                observed.kind,
                state.last_confirmed_kind,
                DESKTOP_KIND_STABLE_FOR.as_millis()
            ));
            state.last_confirmed_kind.unwrap_or(observed.kind)
        }
    }
}

#[cfg(target_os = "windows")]
fn active_input_desktop_kind() -> Option<AgentDesktopKind> {
    let name = crate::capture::gdi::current_input_desktop_name()?;
    if name.eq_ignore_ascii_case("Default") {
        Some(AgentDesktopKind::Default)
    } else {
        Some(AgentDesktopKind::Winlogon)
    }
}

#[cfg(target_os = "windows")]
fn session_is_locked(session_id: u32) -> Option<bool> {
    use windows::core::PWSTR;
    use windows::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW,
        WTS_CURRENT_SERVER_HANDLE, WTS_SESSIONSTATE_LOCK,
    };

    unsafe {
        let mut buffer = PWSTR::null();
        let mut bytes_returned = 0u32;
        if WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes_returned,
        )
        .is_err()
        {
            return None;
        }

        let result = if !buffer.0.is_null()
            && bytes_returned as usize >= std::mem::size_of::<WTSINFOEXW>()
        {
            let info = &*(buffer.0 as *const WTSINFOEXW);
            if info.Level == 1 {
                let level1 = info.Data.WTSInfoExLevel1;
                Some(level1.SessionFlags == WTS_SESSIONSTATE_LOCK as i32)
            } else {
                None
            }
        } else {
            None
        };

        if !buffer.0.is_null() {
            WTSFreeMemory(buffer.0 as *mut std::ffi::c_void);
        }
        result
    }
}

/// Launch the phantom agent process in a specific user session.
/// Get the username associated with a Windows session ID.
#[cfg(target_os = "windows")]
#[allow(dead_code)]
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
/// Prefer the active user's token for the normal desktop. A SYSTEM-token process
/// in the interactive session can capture, but Windows rejects SendInput with
/// access denied on some builds. If no logged-in user token exists yet, fall
/// back to a SYSTEM token in the target session for pre-login/Winlogon capture.
///
/// Returns a WinProcessHandle that owns the process handle for lifecycle management.
#[cfg(target_os = "windows")]
fn launch_agent_in_session(
    session_id: u32,
    desktop_kind: AgentDesktopKind,
) -> anyhow::Result<WinProcessHandle> {
    use anyhow::Context;
    use std::mem;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS, TOKEN_QUERY,
    };
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let exe_path = std::env::current_exe().context("get current exe")?;
        let cmd_line = format!(
            "\"{}\" --agent-mode --ipc-session {} --listen 127.0.0.1:9910 --no-encrypt",
            exe_path.display(),
            session_id,
        );

        if desktop_kind == AgentDesktopKind::Default {
            match launch_agent_with_shell_token(session_id, &cmd_line) {
                Ok(handle) => {
                    svc_log(&format!(
                        "Agent launched with explorer shell token in session {session_id}"
                    ));
                    return Ok(handle);
                }
                Err(e) => {
                    svc_log(&format!(
                        "Explorer shell token launch failed: {e:#}; trying WTS user token"
                    ));
                }
            }

            let mut user_token = HANDLE::default();
            if WTSQueryUserToken(session_id, &mut user_token).is_ok() {
                match create_agent_process_with_token(
                    user_token,
                    &cmd_line,
                    &["winsta0\\default"],
                    "active user",
                ) {
                    Ok(handle) => {
                        let _ = CloseHandle(user_token);
                        svc_log(&format!(
                            "Agent launched with active user token in session {session_id}"
                        ));
                        return Ok(handle);
                    }
                    Err(e) => {
                        let _ = CloseHandle(user_token);
                        svc_log(&format!(
                            "CreateProcessAsUserW with active user token failed: {e:#}; falling back to SYSTEM token"
                        ));
                    }
                }
            } else {
                svc_log(&format!(
                    "WTSQueryUserToken(session {session_id}) failed; falling back to SYSTEM token"
                ));
            }
        } else {
            svc_log(&format!(
                "Session {session_id} targets Winlogon; using SYSTEM token launch"
            ));
        }

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

        let desktop_names: &[&str] = match desktop_kind {
            AgentDesktopKind::Default => &["winsta0\\default"],
            AgentDesktopKind::Winlogon => &["winsta0\\winlogon", "winsta0\\default"],
        };
        let result = create_agent_process_with_token(dup_token, &cmd_line, desktop_names, "SYSTEM");
        let _ = CloseHandle(dup_token);
        result
    }
}

#[cfg(target_os = "windows")]
fn launch_agent_with_shell_token(
    session_id: u32,
    cmd_line: &str,
) -> anyhow::Result<WinProcessHandle> {
    use anyhow::Context;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS,
        TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let explorer_pid = find_process_in_session("explorer.exe", session_id)
        .ok_or_else(|| anyhow::anyhow!("no explorer.exe found in session {session_id}"))?;

    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, explorer_pid)
            .with_context(|| format!("OpenProcess(explorer pid={explorer_pid}) failed"))?;
        let mut shell_token = HANDLE::default();
        let token_result = OpenProcessToken(
            process,
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_IMPERSONATE,
            &mut shell_token,
        );
        let _ = CloseHandle(process);
        token_result
            .with_context(|| format!("OpenProcessToken(explorer pid={explorer_pid}) failed"))?;

        let mut primary_token = HANDLE::default();
        let dup_result = DuplicateTokenEx(
            shell_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary_token,
        );
        let _ = CloseHandle(shell_token);
        dup_result.context("DuplicateTokenEx(explorer token) failed")?;

        let result = create_agent_process_with_token(
            primary_token,
            cmd_line,
            &["winsta0\\default"],
            "explorer shell",
        );
        let _ = CloseHandle(primary_token);
        result
    }
}

#[cfg(target_os = "windows")]
fn find_process_in_session(name: &str, session_id: u32) -> Option<u32> {
    use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        let mut ok = Process32FirstW(snapshot, &mut entry).is_ok();
        while ok {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let exe = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if exe.eq_ignore_ascii_case(name) {
                let mut proc_session = 0u32;
                if ProcessIdToSessionId(entry.th32ProcessID, &mut proc_session).is_ok()
                    && proc_session == session_id
                {
                    found = Some(entry.th32ProcessID);
                    break;
                }
            }
            ok = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

#[cfg(target_os = "windows")]
fn force_kill_process(pid: u32) {
    use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };

    if pid == 0 {
        return;
    }

    unsafe {
        let Ok(process) = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) else {
            svc_log(&format!("OpenProcess(PROCESS_TERMINATE, pid={pid}) failed"));
            return;
        };
        if let Err(e) = TerminateProcess(process, 1) {
            svc_log(&format!(
                "TerminateProcess(pid={pid}) fallback failed: {e:#}"
            ));
            let _ = windows::Win32::Foundation::CloseHandle(process);
            return;
        }
        match WaitForSingleObject(process, 3000) {
            WAIT_OBJECT_0 => svc_log(&format!("Force-killed stale agent PID={pid}")),
            WAIT_TIMEOUT => svc_log(&format!(
                "Stale agent PID={pid} survived force kill timeout"
            )),
            other => svc_log(&format!(
                "WaitForSingleObject(stale PID={pid}) returned {:?}",
                other
            )),
        }
        let _ = windows::Win32::Foundation::CloseHandle(process);
    }
}

#[cfg(target_os = "windows")]
fn kill_lingering_agents_for_other_sessions(target_session_id: u32) {
    use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;

    let current_pid = std::process::id();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return;
        };
        if snapshot == INVALID_HANDLE_VALUE {
            return;
        }

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snapshot, &mut entry).is_ok();
        while ok {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let exe = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if exe.eq_ignore_ascii_case("phantom-server.exe")
                && entry.th32ProcessID != current_pid
                && entry.th32ParentProcessID == current_pid
            {
                let mut proc_session = 0u32;
                if ProcessIdToSessionId(entry.th32ProcessID, &mut proc_session).is_ok()
                    && proc_session != 0
                    && proc_session != target_session_id
                {
                    svc_log(&format!(
                        "Killing lingering phantom agent PID={} session={} before launching session {}",
                        entry.th32ProcessID, proc_session, target_session_id
                    ));
                    force_kill_process(entry.th32ProcessID);
                }
            }
            ok = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
}

#[cfg(target_os = "windows")]
fn create_agent_process_with_token(
    token: windows::Win32::Foundation::HANDLE,
    cmd_line: &str,
    desktop_names: &[&str],
    token_label: &str,
) -> anyhow::Result<WinProcessHandle> {
    use anyhow::Context;
    use std::mem;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        CreateProcessAsUserW, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
        STARTUPINFOW,
    };

    unsafe {
        let mut env_block: *mut std::ffi::c_void = std::ptr::null_mut();
        let _ = windows::Win32::System::Environment::CreateEnvironmentBlock(
            &mut env_block,
            token,
            false,
        );
        let env: Option<*const std::ffi::c_void> = if env_block.is_null() {
            None
        } else {
            Some(env_block as *const std::ffi::c_void)
        };
        let mut last_error: Option<anyhow::Error> = None;
        for desktop_name in desktop_names {
            let mut cmd_wide: Vec<u16> =
                cmd_line.encode_utf16().chain(std::iter::once(0)).collect();
            let mut desktop: Vec<u16> = desktop_name
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut si: STARTUPINFOW = mem::zeroed();
            si.cb = mem::size_of::<STARTUPINFOW>() as u32;
            si.lpDesktop = windows::core::PWSTR(desktop.as_mut_ptr());
            let mut pi: PROCESS_INFORMATION = mem::zeroed();

            let result = CreateProcessAsUserW(
                token,
                None,
                windows::core::PWSTR(cmd_wide.as_mut_ptr()),
                None,
                None,
                false,
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                env,
                None,
                &si,
                &mut pi,
            )
            .with_context(|| format!("CreateProcessAsUserW failed on {desktop_name}"));

            match result {
                Ok(()) => {
                    if !env_block.is_null() {
                        let _ =
                            windows::Win32::System::Environment::DestroyEnvironmentBlock(env_block);
                    }
                    let _ = CloseHandle(pi.hThread);
                    return Ok(WinProcessHandle {
                        handle: pi.hProcess,
                        pid: pi.dwProcessId,
                    });
                }
                Err(e) => {
                    svc_log(&format!(
                        "CreateProcessAsUserW with {token_label} token on {desktop_name} failed: {e:#}"
                    ));
                    last_error = Some(e);
                }
            }
        }

        if !env_block.is_null() {
            let _ = windows::Win32::System::Environment::DestroyEnvironmentBlock(env_block);
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("CreateProcessAsUserW failed")))
    }
}

// ── GPU setup helpers ──────────────────────────────────────────────────────

/// Detect NVIDIA GPU and ensure it's in WDDM mode (required for display rendering).
/// Returns true if a reboot is needed (TCC→WDDM switch requires reboot).
/// Same approach as DCV: auto-detect GPU, auto-switch to WDDM.
fn setup_nvidia_gpu() -> anyhow::Result<bool> {
    // Check if nvidia-smi is available
    let output = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=driver_model.current,name",
            "--format=csv,noheader",
        ])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => {
            println!("  No NVIDIA GPU detected (nvidia-smi not found).");
            return Ok(false);
        }
    };

    let line = output.trim();
    if line.is_empty() {
        println!("  No NVIDIA GPU detected.");
        return Ok(false);
    }

    // Parse "TCC, NVIDIA L40" or "WDDM, NVIDIA A40"
    let parts: Vec<&str> = line.splitn(2, ',').map(|s| s.trim()).collect();
    let (mode, gpu_name) = match parts.as_slice() {
        [mode, name] => (*mode, *name),
        _ => {
            println!("  Could not parse GPU info: {line}");
            return Ok(false);
        }
    };

    println!("  GPU: {gpu_name} (mode: {mode})");

    if mode == "WDDM" {
        println!("  Already in WDDM mode — good.");
        return Ok(false);
    }

    // TCC mode — switch to WDDM for display rendering
    println!("  Switching from TCC to WDDM mode (required for display rendering)...");
    let status = std::process::Command::new("nvidia-smi")
        .args(["-fdm", "0"])
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("  Switched to WDDM mode. Reboot required to take effect.");
            Ok(true) // reboot needed
        }
        Ok(s) => {
            println!("  Warning: nvidia-smi -fdm 0 failed (exit {s}). GPU may stay in TCC mode.");
            Ok(false)
        }
        Err(e) => {
            println!("  Warning: nvidia-smi -fdm 0 failed: {e}");
            Ok(false)
        }
    }
}

/// Disable the Microsoft Basic Display Adapter so Windows uses only the NVIDIA GPU.
#[allow(dead_code)]
fn disable_basic_display_adapter() {
    println!("  Disabling Microsoft Basic Display Adapter...");
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-PnpDevice -Class Display | Where-Object { $_.FriendlyName -like '*Basic Display*' } | Disable-PnpDevice -Confirm:$false -ErrorAction SilentlyContinue",
        ])
        .status();
}

// ── Virtual Display Driver (VDD) helpers ───────────────────────────────────

/// Generate vdd_settings.xml content with desired resolution and GPU.
fn vdd_settings_xml() -> String {
    r#"<?xml version='1.0' encoding='utf-8'?>
<vdd_settings>
    <monitors>
        <count>1</count>
    </monitors>
    <gpu>
        <friendlyname>NVIDIA</friendlyname>
    </gpu>
    <global>
        <g_refresh_rate>60</g_refresh_rate>
    </global>
    <resolutions>
        <resolution><width>640</width><height>480</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>800</width><height>600</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1024</width><height>768</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1152</width><height>864</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1280</width><height>720</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1280</width><height>800</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1280</width><height>960</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1280</width><height>1024</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1366</width><height>768</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1440</width><height>900</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1600</width><height>900</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1600</width><height>1200</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1680</width><height>1050</height><refresh_rate>60</refresh_rate></resolution>
        <resolution><width>1920</width><height>1080</height><refresh_rate>60</refresh_rate></resolution>
    </resolutions>
    <options>
        <CustomEdid>false</CustomEdid>
        <HardwareCursor>true</HardwareCursor>
        <SDR10bit>false</SDR10bit>
        <HDRPlus>false</HDRPlus>
        <logging>false</logging>
        <debuglogging>false</debuglogging>
    </options>
</vdd_settings>"#
        .to_string()
}

/// Download a file using PowerShell (no extra Rust deps needed).
/// Retries up to 3 times with 2s sleep between attempts — first-install on
/// Win11 has occasionally hit a transient TLS / SAS-redirect blip where an
/// immediate manual retry of the exact same URL succeeds.
fn ps_download(url: &str, dest: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
                 $ProgressPreference = 'SilentlyContinue'; \
                 $ErrorActionPreference = 'Stop'; \
                 for ($i = 1; $i -le 3; $i++) {{ \
                   try {{ \
                     Invoke-WebRequest -Uri '{}' -OutFile '{}' -UseBasicParsing; \
                     exit 0 \
                   }} catch {{ \
                     Write-Host \"download attempt $i failed: $_\"; \
                     if ($i -lt 3) {{ Start-Sleep -Seconds 2 }} \
                   }} \
                 }}; \
                 exit 1",
                url,
                dest.display()
            ),
        ])
        .status()
        .context("powershell download")?;
    if !status.success() {
        anyhow::bail!("download failed after 3 attempts: {url}");
    }
    Ok(())
}

/// Extract a zip file using PowerShell.
fn ps_unzip(zip: &std::path::Path, dest: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                zip.display(),
                dest.display()
            ),
        ])
        .status()
        .context("powershell unzip")?;
    if !status.success() {
        anyhow::bail!("unzip failed: {}", zip.display());
    }
    Ok(())
}

/// Install the Virtual Display Driver for headless GPU servers.
/// Downloads VDD + nefcon from GitHub, installs driver via nefconw.
/// Ask pnputil whether any MttVDD device node is currently registered.
/// Used by `install_vdd` to skip the reinstall on upgrade and by
/// callers that want to check state without doing any install work.
pub fn vdd_device_present() -> bool {
    // pnputil /enum-devices shows Device Description + Manufacturer but not
    // the hardware id (Root\MttVDD). The combination "Virtual Display Driver"
    // + "MikeTheTech" is unique enough to identify MTT VDD without false
    // positives from other vendors' virtual display drivers.
    let out = std::process::Command::new("pnputil")
        .args(["/enum-devices", "/connected"])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout).to_lowercase();
            s.contains("virtual display driver") && s.contains("mikethetech")
        }
        Err(_) => false,
    }
}

pub fn install_vdd(install_dir: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;

    // Idempotent: if the MttVDD device node is already present, skip the
    // whole download + nefcon dance. This is the upgrade path — phantom-
    // server `--uninstall` + `--install` was previously causing VDD to
    // disappear for a few seconds, which Windows would respond to by
    // migrating all VDD-hosted windows (browser, IDE, ...) onto the
    // physical display. Those windows save the new position and end up
    // off-screen from phantom's capture viewport. Skipping the reinstall
    // when the driver is already registered avoids the window migration
    // entirely. For a full wipe run `--uninstall-vdd` explicitly.
    if vdd_device_present() {
        println!("  Virtual Display Driver already installed — skipping reinstall");
        println!("  (use --uninstall-vdd to force full removal)");
        return Ok(());
    }

    let vdd_dir = install_dir.join("vdd");
    std::fs::create_dir_all(&vdd_dir).context("create vdd dir")?;

    let tmp = std::env::temp_dir();

    // Download VDD driver
    println!("  Downloading Virtual Display Driver...");
    let vdd_zip = tmp.join("phantom-vdd.zip");
    ps_download(VDD_DRIVER_URL, &vdd_zip)?;
    ps_unzip(&vdd_zip, &tmp.join("phantom-vdd"))?;
    let _ = std::fs::remove_file(&vdd_zip);

    // Copy driver files to install dir
    let extracted = tmp.join("phantom-vdd").join("VirtualDisplayDriver");
    for name in ["MttVDD.dll", "MttVDD.inf", "mttvdd.cat"] {
        let src = extracted.join(name);
        let dst = vdd_dir.join(name);
        std::fs::copy(&src, &dst).with_context(|| format!("copy {name} to vdd dir"))?;
    }

    // Write settings to the path VDD reads from (fixed location).
    let vdd_config_dir = std::path::PathBuf::from(r"C:\VirtualDisplayDriver");
    std::fs::create_dir_all(&vdd_config_dir).context("create VDD config dir")?;
    std::fs::write(vdd_config_dir.join("vdd_settings.xml"), vdd_settings_xml())
        .context("write vdd_settings.xml")?;
    // Also keep a copy in our install dir for reference.
    std::fs::write(vdd_dir.join("vdd_settings.xml"), vdd_settings_xml()).ok();

    // Download nefcon
    println!("  Downloading nefcon (driver installer)...");
    let nefcon_zip = tmp.join("phantom-nefcon.zip");
    ps_download(NEFCON_URL, &nefcon_zip)?;
    ps_unzip(&nefcon_zip, &tmp.join("phantom-nefcon"))?;
    let _ = std::fs::remove_file(&nefcon_zip);

    let nefconw = tmp.join("phantom-nefcon").join("x64").join("nefconw.exe");
    let nefconw_dst = vdd_dir.join("nefconw.exe");
    std::fs::copy(&nefconw, &nefconw_dst).context("copy nefconw.exe")?;

    // Clean up temp dirs
    let _ = std::fs::remove_dir_all(tmp.join("phantom-vdd"));
    let _ = std::fs::remove_dir_all(tmp.join("phantom-nefcon"));

    // Import signing certificates from the .cat file into TrustedPublisher store.
    // Without this, Windows shows a "publisher not trusted" dialog blocking silent install.
    //
    // Pre-create HKLM:\SOFTWARE\Microsoft\SystemCertificates\TrustedPublisher first.
    // On freshly-provisioned Windows images (e.g. cloudbase-init'd VMs that have
    // never opened certlm.msc) this registry key does not exist, and Import-Certificate
    // returns E_ACCESSDENIED — even when running as NT AUTHORITY\SYSTEM. This was
    // previously misdiagnosed as a GPU-mode-transition issue (see git history of
    // pitfalls.md). The only stores Windows initialises by default are Root, MY,
    // Disallowed, etc.; TrustedPublisher is created lazily by certlm.msc / certutil.
    // See: https://learn.microsoft.com/en-us/answers/questions/1679945/
    println!("  Importing driver certificates...");
    let cat_path = vdd_dir.join("mttvdd.cat");
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "$key = 'HKLM:\\SOFTWARE\\Microsoft\\SystemCertificates\\TrustedPublisher'; \
                 if (-not (Test-Path $key)) {{ New-Item -Path $key -Force | Out-Null }}; \
                 Add-Type -AssemblyName System.Security; \
                 $cms = New-Object System.Security.Cryptography.Pkcs.SignedCms; \
                 $cms.Decode([System.IO.File]::ReadAllBytes('{}')); \
                 foreach ($cert in $cms.Certificates) {{ \
                     $f = [System.IO.Path]::GetTempFileName() + '.cer'; \
                     [System.IO.File]::WriteAllBytes($f, $cert.Export(\
                         [System.Security.Cryptography.X509Certificates.X509ContentType]::Cert)); \
                     Import-Certificate -FilePath $f -CertStoreLocation 'Cert:\\LocalMachine\\TrustedPublisher' | Out-Null; \
                     Remove-Item $f \
                 }}",
                cat_path.display()
            ),
        ])
        .status();

    // Remove ALL existing VDD device nodes before installing a fresh one.
    // The previous code ran `nefconw --remove-device-node` exactly once,
    // which only removes the first matching device. Any extra instances
    // left over from a partial/failed previous install stayed, and the
    // new `nefconw install` below added another on top — so repeated
    // uninstall→install cycles accumulated duplicate Virtual Display
    // Driver entries in Win32_VideoController. Now we enumerate via
    // pnputil and remove every one before installing.
    println!("  Removing old VDD device nodes...");
    let enum_out = std::process::Command::new("pnputil")
        .args(["/enum-devices", "/connected"])
        .output();
    let mut removed = 0usize;
    if let Ok(out) = enum_out {
        let text = String::from_utf8_lossy(&out.stdout);
        // pnputil groups fields per device. "Instance ID: ROOT\MTTVDD\..."
        // identifies VDD devices; there's one line per device, and the
        // preceding Instance ID line is what `pnputil /remove-device`
        // wants.
        for line in text.lines() {
            let t = line.trim();
            if let Some(id) = t.strip_prefix("Instance ID:") {
                let id = id.trim();
                if id.to_ascii_uppercase().contains("MTTVDD") {
                    let r = std::process::Command::new("pnputil")
                        .args(["/remove-device", id])
                        .status();
                    match r {
                        Ok(s) if s.success() => removed += 1,
                        Ok(s) => println!(
                            "  Warning: pnputil /remove-device {id} exit code {:?}",
                            s.code()
                        ),
                        Err(e) => println!("  Warning: pnputil spawn failed: {e}"),
                    }
                }
            }
        }
    }
    // Belt-and-suspenders: also call nefconw's by-hardware-id flavour in
    // case pnputil missed anything (different Windows builds surface
    // the instance list differently).
    let _ = std::process::Command::new(&nefconw_dst)
        .args([
            "--remove-device-node",
            "--hardware-id",
            VDD_HARDWARE_ID,
            "--class-guid",
            VDD_CLASS_GUID,
        ])
        .status();
    if removed > 0 {
        println!("  Removed {removed} existing VDD device node(s).");
    }

    // Install driver using nefcon (devcon-compatible syntax: install <inf> <hwid>).
    // This creates exactly one device node + installs the driver.
    println!("  Installing Virtual Display Driver...");
    let inf_path = vdd_dir.join("MttVDD.inf");
    let status = std::process::Command::new(&nefconw_dst)
        .args(["install", &inf_path.to_string_lossy(), VDD_HARDWARE_ID])
        .status()
        .context("nefconw install")?;

    // Exit code 0 = success, 3010 = success but reboot required.
    let code = status.code().unwrap_or(-1);
    if code == 0 || code == 3010 {
        println!("  Virtual Display Driver installed (1920x1080 default).");
        if code == 3010 {
            println!("  Note: A reboot may be required for the display to appear.");
        }
    } else {
        println!("  Warning: Virtual Display Driver install returned exit code {code}.");
        println!("  The server will still work but may capture at low resolution on headless VMs.");
    }

    Ok(())
}

/// Uninstall the Virtual Display Driver.
pub fn uninstall_vdd(install_dir: &std::path::Path) -> anyhow::Result<()> {
    let vdd_dir = install_dir.join("vdd");
    let nefconw = vdd_dir.join("nefconw.exe");

    // Step 1: remove the device node via nefconw (if available).
    // Previously this swallowed the exit code and always claimed success;
    // we now at least log whether nefconw thought it worked.
    if nefconw.exists() {
        println!("  Removing Virtual Display Driver device node...");
        match std::process::Command::new(&nefconw)
            .args([
                "--remove-device-node",
                "--hardware-id",
                VDD_HARDWARE_ID,
                "--class-guid",
                VDD_CLASS_GUID,
            ])
            .status()
        {
            Ok(s) if s.success() => {}
            Ok(s) => {
                println!(
                    "  Warning: nefconw --remove-device-node exit code {:?}",
                    s.code()
                );
            }
            Err(e) => {
                println!("  Warning: nefconw failed to spawn: {e}");
            }
        }
    }

    // Step 2: remove the driver package from the Windows driver store.
    // `nefconw --remove-device-node` only removes the device node — the .inf
    // stays in the driver store, so pnputil sees it and the next install
    // pulls from cache. Use pnputil to enumerate and delete any oem*.inf
    // that mentions our hardware id.
    println!("  Removing VDD driver package from driver store...");
    let enum_output = std::process::Command::new("pnputil")
        .args(["/enum-drivers"])
        .output();
    if let Ok(out) = enum_output {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut published_names: Vec<String> = Vec::new();
        let mut cur_name: Option<String> = None;
        // pnputil output is "Published Name: oem42.inf" blocks separated by
        // an "Original Name" / "Provider Name" / etc. We walk line-by-line:
        // remember the most recent Published Name; on a line containing
        // "MttVDD" (in either "Original Name" or "Hardware ID" subsection
        // after running /enum-drivers) associate it with that name.
        for line in text.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("Published Name:") {
                cur_name = Some(rest.trim().to_string());
            } else if t.to_ascii_lowercase().contains("mttvdd") {
                if let Some(ref n) = cur_name {
                    if !published_names.contains(n) {
                        published_names.push(n.clone());
                    }
                }
            }
        }
        for name in &published_names {
            println!("  Deleting driver package {name}...");
            let r = std::process::Command::new("pnputil")
                .args(["/delete-driver", name, "/uninstall", "/force"])
                .status();
            match r {
                Ok(s) if s.success() => {}
                Ok(s) => println!(
                    "  Warning: pnputil /delete-driver {name} exit code {:?}",
                    s.code()
                ),
                Err(e) => println!("  Warning: pnputil spawn failed: {e}"),
            }
        }
        if published_names.is_empty() {
            println!("  (no MttVDD driver packages found in store)");
        }
    } else {
        println!("  Warning: pnputil /enum-drivers failed to run");
    }

    // Step 3: verify no MttVDD device is left. Purely diagnostic — users
    // see this and know whether to reboot / retry / ignore.
    let verify = std::process::Command::new("pnputil")
        .args(["/enum-devices"])
        .output();
    if let Ok(out) = verify {
        let text = String::from_utf8_lossy(&out.stdout);
        if text.to_ascii_lowercase().contains("mttvdd") {
            println!("  Warning: pnputil still lists an MttVDD device — reboot may be needed.");
        } else {
            println!("  VDD fully removed.");
        }
    }

    // Step 4: clean up our own install files + config dir.
    let _ = std::fs::remove_dir_all(&vdd_dir);
    let _ = std::fs::remove_dir_all(r"C:\VirtualDisplayDriver");

    Ok(())
}

// ── Service installation helpers ────────────────────────────────────────────

fn query_service_text() -> Option<String> {
    let output = std::process::Command::new("sc")
        .args(["query", SERVICE_NAME])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn service_exists() -> bool {
    query_service_text().is_some()
}

fn service_is_running() -> bool {
    query_service_text()
        .as_deref()
        .is_some_and(|text| text.contains("RUNNING"))
}

fn stop_service_for_update() -> anyhow::Result<()> {
    if !service_exists() {
        return Ok(());
    }

    println!("Stopping existing {SERVICE_DISPLAY_NAME} service...");
    let _ = std::process::Command::new("sc")
        .args(["stop", SERVICE_NAME])
        .status();

    for _ in 0..30 {
        if !service_is_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    println!("  Service did not stop quickly; force-killing service process...");
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/FI", &format!("SERVICES eq {SERVICE_NAME}")])
        .status();

    for _ in 0..10 {
        if !service_is_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    anyhow::bail!("existing {SERVICE_NAME} service is still running after stop/kill")
}

fn kill_other_phantom_server_processes() {
    let my_pid = std::process::id();
    let _ = std::process::Command::new("taskkill")
        .args([
            "/F",
            "/FI",
            "IMAGENAME eq phantom-server.exe",
            "/FI",
            &format!("PID ne {my_pid}"),
        ])
        .status();
    std::thread::sleep(Duration::from_secs(1));
}

fn configure_crash_dumps() {
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    let dump_dir = std::path::Path::new(&program_data)
        .join("Phantom")
        .join("Dumps");
    if let Err(e) = std::fs::create_dir_all(&dump_dir) {
        println!("  Warning: could not create crash dump directory: {e}");
        return;
    }

    let key =
        r"HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\phantom-server.exe";
    let dump_dir_s = dump_dir.to_string_lossy().to_string();
    reg_add(&[
        "add",
        key,
        "/v",
        "DumpFolder",
        "/t",
        "REG_EXPAND_SZ",
        "/d",
        dump_dir_s.as_str(),
        "/f",
    ]);
    reg_add(&[
        "add",
        key,
        "/v",
        "DumpType",
        "/t",
        "REG_DWORD",
        "/d",
        "2",
        "/f",
    ]);
    reg_add(&[
        "add",
        key,
        "/v",
        "DumpCount",
        "/t",
        "REG_DWORD",
        "/d",
        "5",
        "/f",
    ]);
}

fn reg_add(args: &[&str]) {
    match std::process::Command::new("reg").args(args).status() {
        Ok(status) if status.success() => {}
        Ok(status) => println!("  Warning: reg add for crash dumps failed with {status}"),
        Err(e) => println!("  Warning: reg add for crash dumps failed: {e}"),
    }
}

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
    let service_already_exists = service_exists();

    if service_already_exists {
        stop_service_for_update()?;
        kill_other_phantom_server_processes();
    }

    let src_exe = std::env::current_exe().context("get current exe path")?;
    let same_exe = src_exe
        .to_string_lossy()
        .eq_ignore_ascii_case(&install_exe.to_string_lossy());
    if same_exe {
        println!("  Running from install location; service binary is already in place.");
    } else {
        std::fs::copy(&src_exe, &install_exe).context(
            "copy exe to install dir (could not update service binary after stopping service)",
        )?;
    }

    let bin_path = format!("\"{}\" --service", install_exe.display());
    if service_already_exists {
        println!("Updating existing {SERVICE_DISPLAY_NAME} service...");
        let status = std::process::Command::new("sc")
            .args([
                "config",
                SERVICE_NAME,
                "binPath=",
                &bin_path,
                "start=",
                "auto",
                "obj=",
                "LocalSystem",
            ])
            .status()
            .context("sc config")?;
        if !status.success() {
            anyhow::bail!("sc config failed with {status}. Run as Administrator.");
        }
    } else {
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
    }

    // Set description
    let _ = std::process::Command::new("sc")
        .args(["description", SERVICE_NAME, SERVICE_DESCRIPTION])
        .status();

    // Auto-restart after crashes. `sc stop` still stops cleanly because this
    // policy applies to unexpected failures, not normal service stops.
    let _ = std::process::Command::new("sc")
        .args([
            "failure",
            SERVICE_NAME,
            "reset=",
            "86400",
            "actions=",
            "restart/5000/restart/5000/restart/30000",
        ])
        .status();
    let _ = std::process::Command::new("sc")
        .args(["failureflag", SERVICE_NAME, "1"])
        .status();
    configure_crash_dumps();

    // ── Windows Firewall inbound rule ──
    // Without this, the service listens on 9900 but `DefaultInboundAction`
    // (NotConfigured → Block) silently drops remote connections. Add a
    // program-scoped allow rule so the exe is reachable from the network.
    println!();
    println!("Adding Windows Firewall inbound rule...");
    match install_firewall_rule(&install_exe) {
        Ok(()) => println!(
            "  Firewall rule '{SERVICE_NAME}' added (inbound allow for phantom-server.exe)."
        ),
        Err(e) => {
            println!("  Warning: firewall rule add failed: {e}");
            println!("  Remote clients may be blocked by Windows Firewall until you add a rule manually.");
        }
    }

    // ── GPU setup (like DCV: auto-detect, auto-configure) ──
    println!();
    println!("Configuring GPU...");
    let needs_reboot = match setup_nvidia_gpu() {
        Ok(reboot) => reboot,
        Err(e) => {
            println!("  Warning: GPU setup failed: {e}");
            false
        }
    };

    // Install Virtual Display Driver (for headless GPU servers).
    // Non-fatal: server works without it, just at lower resolution on headless VMs.
    println!();
    println!("Installing Virtual Display Driver...");
    match install_vdd(&install_dir) {
        Ok(()) => {}
        Err(e) => {
            println!("  Warning: VDD install failed: {e}");
            println!("  The server will still work. Install VDD manually if needed.");
        }
    }

    // NOTE: do NOT disable Basic Display Adapter — it causes boot failure
    // on reboot. The VDD approach works without disabling other displays.
    // DXGI targets VDD by device name, so other displays don't interfere.

    println!();
    println!("Installed: {SERVICE_DISPLAY_NAME} (Windows Service)");
    println!("  The service runs at boot as LocalSystem (Session 0).");
    println!("  Remote access works even before user login.");
    println!();

    if needs_reboot {
        println!("  *** REBOOT REQUIRED ***");
        println!("  GPU was switched from TCC to WDDM mode.");
        println!("  Run: shutdown /r /t 10");
        println!("  After reboot, run --install again to finalize GPU display setup.");
        println!();
        println!("  To check status: sc query {SERVICE_NAME}");
        println!("  To remove:       phantom-server --uninstall");
    } else {
        // Start the service immediately (no reboot needed)
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
    }

    Ok(())
}

/// Add an inbound Windows Firewall rule for phantom-server.exe.
///
/// The rule is program-scoped (not port-scoped) so it still works when the
/// user overrides `--port` or enables QUIC (UDP) / WebRTC. Without this,
/// Windows Firewall's default `NotConfigured` inbound action silently drops
/// all connections to the service, and the user sees a confusing "service
/// running but nothing connects" state.
///
/// Uses `netsh` (not PowerShell's `New-NetFirewallRule`) to match the rest
/// of the install flow, which already uses `sc.exe` / `schtasks.exe` —
/// avoids one more dependency on the PS execution environment.
fn install_firewall_rule(program: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;

    let _ = std::process::Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={SERVICE_NAME}"),
        ])
        .status();

    let status = std::process::Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name={SERVICE_NAME}"),
            "dir=in",
            "action=allow",
            &format!("program={}", program.display()),
            "enable=yes",
            "profile=any",
            &format!(
                "description={SERVICE_DISPLAY_NAME} — allow inbound connections to phantom-server.exe"
            ),
        ])
        .status()
        .context("netsh advfirewall firewall add rule")?;

    if !status.success() {
        anyhow::bail!("netsh add rule failed with {status}. Run as Administrator.");
    }

    Ok(())
}

/// Remove the inbound Windows Firewall rule added by `install_firewall_rule`.
/// Silent on missing rule — this is called from `--uninstall` which must
/// succeed on partial installs.
fn uninstall_firewall_rule() {
    let _ = std::process::Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={SERVICE_NAME}"),
        ])
        .status();
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

    // Force kill fallback — if sc stop didn't work, kill the process directly.
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/FI", &format!("SERVICES eq {SERVICE_NAME}")])
        .status();
    // Also kill any agent processes — but NEVER the current process.
    // The uninstall binary runs as phantom-server.exe itself, so an
    // unconditional `taskkill /IM phantom-server.exe` would suicide
    // before reaching `sc delete` / `uninstall_vdd` below. Exclude our
    // own PID via a second filter.
    let my_pid = std::process::id();
    let _ = std::process::Command::new("taskkill")
        .args([
            "/F",
            "/FI",
            "IMAGENAME eq phantom-server.exe",
            "/FI",
            &format!("PID ne {my_pid}"),
        ])
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

    uninstall_firewall_rule();

    // Intentionally *not* removing the Virtual Display Driver here — we
    // want `--uninstall` to be safe to run as part of an upgrade. The
    // previous behaviour (always removing VDD) caused every upgrade to
    // shuffle user windows onto the physical display while VDD was
    // briefly absent, leaving those windows off-screen after reinstall.
    // For a full wipe, operators call `--uninstall-vdd` explicitly.
    if vdd_device_present() {
        println!("  (Virtual Display Driver left in place; use --uninstall-vdd to remove it)");
    }

    // Clean up schtasks
    let _ = std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", "PhantomServer", "/F"])
        .status();

    Ok(())
}
