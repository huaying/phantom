//! Wayland screen capture via XDG Desktop Portal + PipeWire.
//!
//! Flow:
//! 1. D-Bus → org.freedesktop.portal.ScreenCast → CreateSession → SelectSources → Start
//! 2. Portal returns PipeWire node ID + file descriptor
//! 3. PipeWire stream connects to node, receives raw video frames (BGRx/BGRA)
//! 4. Frames are converted to our Frame struct (BGRA8)
//!
//! Feature-gated behind `wayland` feature flag.
//! Requires: libpipewire-0.3-dev, xdg-desktop-portal running.

use anyhow::{Context, Result, bail};
use phantom_core::capture::FrameCapture;
use phantom_core::frame::{Frame, PixelFormat};
use std::os::fd::{FromRawFd, OwnedFd, IntoRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// PipeWire-based screen capture for Wayland sessions.
pub struct PipeWireCapture {
    width: u32,
    height: u32,
    /// Shared frame buffer: PipeWire callback writes here, capture() reads.
    frame_buffer: Arc<Mutex<Option<FrameData>>>,
    /// Set to true when PipeWire stream is active.
    #[allow(dead_code)]
    running: Arc<AtomicBool>,
    /// Handle to the PipeWire thread (for cleanup).
    _pw_thread: std::thread::JoinHandle<()>,
}

struct FrameData {
    data: Vec<u8>,
    width: u32,
    height: u32,
    timestamp: Instant,
}

/// Information returned by the XDG Desktop Portal ScreenCast session.
struct PortalSession {
    pw_fd: OwnedFd,
    pw_node_id: u32,
    width: u32,
    height: u32,
}

impl PipeWireCapture {
    pub fn new() -> Result<Self> {
        tracing::info!("initializing PipeWire Wayland capture");

        // Step 1: Create portal session and get PipeWire fd + node id
        let portal = create_portal_session()
            .context("failed to create XDG ScreenCast portal session")?;

        let width = portal.width;
        let height = portal.height;
        tracing::info!(width, height, node_id = portal.pw_node_id, "portal session created");

        let frame_buffer: Arc<Mutex<Option<FrameData>>> = Arc::new(Mutex::new(None));
        let running = Arc::new(AtomicBool::new(false));

        // Step 2: Start PipeWire stream in a dedicated thread
        let fb = Arc::clone(&frame_buffer);
        let run = Arc::clone(&running);
        let node_id = portal.pw_node_id;

        // Transfer fd ownership to the thread
        let pw_fd_raw = portal.pw_fd.into_raw_fd();

        let pw_thread = std::thread::Builder::new()
            .name("pipewire-capture".into())
            .spawn(move || {
                let pw_fd = unsafe { OwnedFd::from_raw_fd(pw_fd_raw) };
                if let Err(e) = run_pipewire_stream(pw_fd, node_id, fb, run) {
                    tracing::error!("PipeWire stream error: {e}");
                }
            })
            .context("failed to spawn PipeWire thread")?;

        // Wait for stream to start (up to 5s)
        let deadline = Instant::now() + Duration::from_secs(5);
        while !running.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if !running.load(Ordering::Relaxed) {
            bail!("PipeWire stream failed to start within 5 seconds");
        }

        tracing::info!("PipeWire capture initialized");

        Ok(Self {
            width,
            height,
            frame_buffer,
            running,
            _pw_thread: pw_thread,
        })
    }
}

impl FrameCapture for PipeWireCapture {
    fn capture(&mut self) -> Result<Option<Frame>> {
        let mut guard = self.frame_buffer.lock().unwrap();
        match guard.take() {
            Some(fd) => {
                self.width = fd.width;
                self.height = fd.height;
                Ok(Some(Frame {
                    width: fd.width,
                    height: fd.height,
                    format: PixelFormat::Bgra8,
                    data: fd.data,
                    timestamp: fd.timestamp,
                }))
            }
            None => Ok(None),
        }
    }

    fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn reset(&mut self) -> Result<()> {
        let mut guard = self.frame_buffer.lock().unwrap();
        *guard = None;
        Ok(())
    }
}

// ── XDG Desktop Portal ScreenCast via D-Bus ─────────────────────────────────
//
// Uses the `dbus` crate (synchronous) to talk to xdg-desktop-portal.
// The portal may show a user dialog to select which screen to share.

fn create_portal_session() -> Result<PortalSession> {
    use dbus::arg::{OwnedFd as DBusFd, RefArg, Variant};
    use dbus::blocking::{Connection, Proxy};
    use std::collections::HashMap;

    let conn = Connection::new_session().context("failed to connect to session D-Bus")?;

    let portal = Proxy::new(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        Duration::from_secs(30),
        &conn,
    );

    // Generate a unique token for request/session paths
    let token = format!("phantom_{}", std::process::id());
    let session_token = format!("phantom_session_{}", std::process::id());

    // CreateSession
    let mut create_opts: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
    create_opts.insert(
        "handle_token".into(),
        Variant(Box::new(token.clone())),
    );
    create_opts.insert(
        "session_handle_token".into(),
        Variant(Box::new(session_token.clone())),
    );

    let (session_path,): (dbus::Path,) = portal
        .method_call(
            "org.freedesktop.portal.ScreenCast",
            "CreateSession",
            (create_opts,),
        )
        .context("CreateSession failed")?;

    tracing::debug!(session = %session_path, "portal CreateSession returned request path");

    // Wait for the Response signal
    let session_handle = wait_for_portal_response(&conn, &session_path)
        .context("CreateSession response failed")?
        .get("session_handle")
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| {
            format!(
                "/org/freedesktop/portal/desktop/session/{}/{}",
                conn.unique_name()
                    .trim_start_matches(':')
                    .replace('.', "_"),
                session_token
            )
        });

    let session_handle = dbus::Path::from(session_handle);
    tracing::debug!(session = %session_handle, "portal session established");

    // SelectSources
    let mut select_opts: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
    select_opts.insert(
        "handle_token".into(),
        Variant(Box::new(format!("{token}_select"))),
    );
    // types: 1 = MONITOR, 2 = WINDOW
    select_opts.insert("types".into(), Variant(Box::new(1u32)));
    // cursor_mode: 2 = EMBEDDED (cursor drawn into the stream)
    select_opts.insert("cursor_mode".into(), Variant(Box::new(2u32)));
    select_opts.insert("multiple".into(), Variant(Box::new(false)));

    let (select_request,): (dbus::Path,) = portal
        .method_call(
            "org.freedesktop.portal.ScreenCast",
            "SelectSources",
            (&session_handle, select_opts),
        )
        .context("SelectSources failed")?;

    let _select_results = wait_for_portal_response(&conn, &select_request)
        .context("SelectSources response failed")?;

    // Start (this may show a user dialog)
    let mut start_opts: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
    start_opts.insert(
        "handle_token".into(),
        Variant(Box::new(format!("{token}_start"))),
    );

    let (start_request,): (dbus::Path,) = portal
        .method_call(
            "org.freedesktop.portal.ScreenCast",
            "Start",
            (&session_handle, "", start_opts),
        )
        .context("Start failed")?;

    let start_results = wait_for_portal_response(&conn, &start_request)
        .context("Start response failed (user may have denied access)")?;

    // Parse streams from response
    let streams = start_results
        .get("streams")
        .context("no 'streams' in Start response")?;

    // streams is a(ua{sv}) — array of (node_id, properties)
    let streams_iter = streams
        .as_iter()
        .context("streams is not iterable")?;

    let mut node_id: Option<u32> = None;
    let mut width = 1920u32;
    let mut height = 1080u32;

    // The outer array contains structs; each struct has (u32, dict)
    // dbus crate represents this as alternating elements in the iterator
    for item in streams_iter {
        // Try to get the node_id from the first element
        if node_id.is_none() {
            if let Some(id) = item.as_u64() {
                node_id = Some(id as u32);
                continue;
            }
            // It might be a struct/array wrapper — try to iterate into it
            if let Some(mut inner) = item.as_iter() {
                if let Some(first) = inner.next() {
                    if let Some(id) = first.as_u64() {
                        node_id = Some(id as u32);
                    }
                    // Try to get size from the properties dict
                    if let Some(props) = inner.next() {
                        if let Some(props_iter) = props.as_iter() {
                            for prop in props_iter {
                                if let Some(mut kv) = prop.as_iter() {
                                    if let (Some(key), Some(val)) = (kv.next(), kv.next()) {
                                        if key.as_str() == Some("size") {
                                            if let Some(mut size_iter) = val.as_iter() {
                                                if let (Some(w), Some(h)) = (size_iter.next(), size_iter.next()) {
                                                    if let (Some(w), Some(h)) = (w.as_u64(), h.as_u64()) {
                                                        width = w as u32;
                                                        height = h as u32;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let node_id = node_id.context("failed to extract PipeWire node ID from portal response")?;
    tracing::info!(node_id, width, height, "portal stream selected");

    // OpenPipeWireRemote: get the file descriptor
    let open_opts: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
    let (pw_fd,): (DBusFd,) = portal
        .method_call(
            "org.freedesktop.portal.ScreenCast",
            "OpenPipeWireRemote",
            (&session_handle, open_opts),
        )
        .context("OpenPipeWireRemote failed")?;

    let fd = unsafe { OwnedFd::from_raw_fd(pw_fd.into_fd()) };

    Ok(PortalSession {
        pw_fd: fd,
        pw_node_id: node_id,
        width,
        height,
    })
}

/// Wait for a portal Response signal on the given request path.
/// Returns the response properties or an error if the user denied.
fn wait_for_portal_response(
    conn: &dbus::blocking::Connection,
    request_path: &dbus::Path,
) -> Result<std::collections::HashMap<String, Box<dyn dbus::arg::RefArg>>> {
    use dbus::arg::RefArg;
    use dbus::message::MatchRule;

    let rule = MatchRule::new_signal("org.freedesktop.portal.Request", "Response")
        .with_path(request_path.clone());

    let rule_str = rule.match_str();
    conn.add_match_no_cb(&rule_str)
        .context("failed to add D-Bus match rule")?;

    // Poll for up to 60 seconds (user might need to interact with a dialog)
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() > deadline {
            bail!("portal response timeout (60s)");
        }

        conn.process(Duration::from_millis(200))
            .context("D-Bus process failed")?;

        // Check for incoming signals
        if let Some(msg) = conn.channel().pop_message() {
            use dbus::message::MessageType;
            if msg.msg_type() == MessageType::Signal {
                if let Some(interface) = msg.interface() {
                    if &*interface == "org.freedesktop.portal.Request" {
                        if let Some(member) = msg.member() {
                            if &*member == "Response" {
                                let _ = conn.remove_match_no_cb(&rule_str);

                                // Parse: (u, a{sv})
                                let (response_code, results): (
                                    u32,
                                    std::collections::HashMap<String, dbus::arg::Variant<Box<dyn RefArg>>>,
                                ) = msg.read2().context("failed to parse Response signal")?;

                                if response_code != 0 {
                                    bail!("portal returned error code {response_code} (1=cancelled, 2=other)");
                                }

                                // Unwrap Variant wrappers
                                let mut map: std::collections::HashMap<String, Box<dyn RefArg>> =
                                    std::collections::HashMap::new();
                                for (k, v) in results {
                                    map.insert(k, v.0);
                                }
                                return Ok(map);
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── PipeWire stream ─────────────────────────────────────────────────────────

fn run_pipewire_stream(
    pw_fd: OwnedFd,
    node_id: u32,
    frame_buffer: Arc<Mutex<Option<FrameData>>>,
    running: Arc<AtomicBool>,
) -> Result<()> {
    use pipewire as pw;
    use pw::spa;

    pw::init();

    let mainloop = pw::main_loop::MainLoop::new(None)
        .context("failed to create PipeWire MainLoop")?;
    let context = pw::context::Context::new(&mainloop)
        .context("failed to create PipeWire Context")?;

    // Connect using the portal's file descriptor
    let core = context
        .connect_fd(pw_fd, None)
        .context("failed to connect PipeWire core with portal fd")?;

    let stream = pw::stream::Stream::new(
        &core,
        "phantom-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("failed to create PipeWire Stream")?;

    // Build format params: request BGRx (= BGRA with alpha=0xff)
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::RGBA
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle { width: 1920, height: 1080 },
            pw::spa::utils::Rectangle { width: 1, height: 1 },
            pw::spa::utils::Rectangle { width: 7680, height: 4320 }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 60, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 144, denom: 1 }
        ),
    );

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow::anyhow!("PipeWire pod serialize error: {e:?}"))?
    .0
    .into_inner();

    let mut params = [pw::spa::pod::Pod::from_bytes(&values)
        .context("failed to create PipeWire format pod")?];

    let fb = frame_buffer;
    let run = running;
    let ml = mainloop.clone();

    let _listener = stream
        .add_local_listener::<()>()
        .state_changed(move |_, _, old, new| {
            tracing::debug!("PipeWire stream state: {:?} → {:?}", old, new);
            match new {
                pw::stream::StreamState::Streaming => {
                    run.store(true, Ordering::Relaxed);
                }
                pw::stream::StreamState::Error(_) => {
                    tracing::error!("PipeWire stream error state");
                    run.store(false, Ordering::Relaxed);
                    ml.quit();
                }
                _ => {}
            }
        })
        .param_changed({
            move |_, _, id, param| {
                let Some(param) = param else { return };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let (media_type, media_subtype) =
                    match pw::spa::param::format_utils::parse_format(param) {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                if media_type != pw::spa::param::format::MediaType::Video
                    || media_subtype != pw::spa::param::format::MediaSubtype::Raw
                {
                    return;
                }
                let mut format = spa::param::video::VideoInfoRaw::default();
                if format.parse(param).is_ok() {
                    tracing::info!(
                        "PipeWire negotiated: {:?} {}x{} @ {}/{}fps",
                        format.format(),
                        format.size().width,
                        format.size().height,
                        format.framerate().num,
                        format.framerate().denom,
                    );
                }
            }
        })
        .process({
            let fb = fb.clone();
            move |stream, _| {
                if let Some(mut buffer) = stream.dequeue_buffer() {
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        return;
                    }
                    let data = &mut datas[0];
                    let chunk_size = data.chunk().size() as usize;
                    let chunk_stride = data.chunk().stride() as usize;
                    if chunk_size == 0 {
                        return;
                    }

                    if let Some(slice) = data.data() {
                        let frame_bytes = &slice[..chunk_size.min(slice.len())];

                        // Calculate dimensions from stride (4 bytes per pixel for BGRx/BGRA)
                        let (w, h) = if chunk_stride >= 4 {
                            let w = (chunk_stride / 4) as u32;
                            let h = (chunk_size / chunk_stride) as u32;
                            (w, h)
                        } else {
                            return; // can't determine dimensions
                        };

                        // Copy frame data, removing any stride padding if needed
                        let pixel_stride = w as usize * 4;
                        let buf = if pixel_stride == chunk_stride {
                            // No padding, direct copy
                            frame_bytes.to_vec()
                        } else {
                            // Has padding — copy row by row (unlikely for BGRx)
                            let mut buf = Vec::with_capacity(pixel_stride * h as usize);
                            for y in 0..h as usize {
                                let start = y * chunk_stride;
                                let end = start + pixel_stride;
                                if end <= frame_bytes.len() {
                                    buf.extend_from_slice(&frame_bytes[start..end]);
                                }
                            }
                            buf
                        };

                        *fb.lock().unwrap() = Some(FrameData {
                            data: buf,
                            width: w,
                            height: h,
                            timestamp: Instant::now(),
                        });
                    }
                }
            }
        })
        .register()?;

    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    tracing::info!("PipeWire stream connected, entering main loop");
    mainloop.run();

    Ok(())
}
