use anyhow::{Context, Result};
use phantom_core::capture::FrameCapture;
use phantom_core::frame::{Frame, PixelFormat};
use scrap::{Capturer, Display};
use std::time::Instant;

/// CPU-based screen capture using the `scrap` crate.
/// Uses DXGI (Windows), Core Graphics (macOS), X11 (Linux).
pub struct ScrapCapture {
    capturer: Option<Capturer>,
    width: u32,
    height: u32,
    display_index: usize,
    #[cfg(target_os = "linux")]
    x11_fallback: Option<X11GetImageCapture>,
    #[cfg(target_os = "linux")]
    x11_fallback_active: bool,
}

impl ScrapCapture {
    /// Create a capturer for the primary display.
    #[allow(dead_code)]
    pub fn new() -> Result<Self> {
        Self::with_display(0)
    }

    /// Create a capturer for a specific display by index.
    ///
    /// Index 0 is the primary display. Use `list_displays()` to enumerate.
    pub fn with_display(index: usize) -> Result<Self> {
        let displays = Display::all().context("failed to enumerate displays")?;
        if displays.is_empty() {
            anyhow::bail!("no displays found");
        }
        if index >= displays.len() {
            anyhow::bail!(
                "display index {index} out of range (found {} display{})",
                displays.len(),
                if displays.len() == 1 { "" } else { "s" }
            );
        }

        let display = displays
            .into_iter()
            .nth(index)
            .context("display disappeared during enumeration")?;
        let width = display.width() as u32;
        let height = display.height() as u32;
        let capturer = Capturer::new(display).context("failed to create capturer")?;
        #[cfg(target_os = "linux")]
        let x11_fallback = match X11GetImageCapture::new(width, height) {
            Ok(capture) => {
                tracing::info!("Linux scrap capture will use X11 GetImage fallback");
                Some(capture)
            }
            Err(e) => {
                tracing::debug!("X11 GetImage fallback unavailable: {e}");
                None
            }
        };
        #[cfg(target_os = "linux")]
        let x11_fallback_active = x11_fallback.is_some();
        tracing::info!(index, width, height, "ScrapCapture initialized");
        Ok(Self {
            capturer: Some(capturer),
            width,
            height,
            display_index: index,
            #[cfg(target_os = "linux")]
            x11_fallback,
            #[cfg(target_os = "linux")]
            x11_fallback_active,
        })
    }

    /// Recreate the capturer. Resets DXGI Desktop Duplication state so the
    /// next capture() call returns a frame even on a static desktop.
    pub fn reset(&mut self) -> Result<()> {
        let displays = Display::all().context("failed to enumerate displays")?;
        let index = self.display_index.min(displays.len().saturating_sub(1));
        let display = displays
            .into_iter()
            .nth(index)
            .context("display disappeared during reset")?;
        self.width = display.width() as u32;
        self.height = display.height() as u32;
        // Drop old capturer BEFORE creating new — only one DuplicateOutput per output.
        self.capturer = None;
        self.capturer = Some(Capturer::new(display).context("failed to recreate capturer")?);
        self.display_index = index;
        #[cfg(target_os = "linux")]
        {
            self.x11_fallback = match X11GetImageCapture::new(self.width, self.height) {
                Ok(capture) => {
                    tracing::info!("Linux scrap capture will use X11 GetImage fallback");
                    Some(capture)
                }
                Err(e) => {
                    tracing::debug!("X11 GetImage fallback unavailable after reset: {e}");
                    None
                }
            };
            self.x11_fallback_active = self.x11_fallback.is_some();
        }
        tracing::debug!(index, "ScrapCapture reset");
        Ok(())
    }

    /// List all available displays with their index and resolution.
    pub fn list_displays() -> Result<Vec<DisplayInfo>> {
        let displays = Display::all().context("failed to enumerate displays")?;
        Ok(displays
            .iter()
            .enumerate()
            .map(|(i, d)| DisplayInfo {
                index: i,
                width: d.width() as u32,
                height: d.height() as u32,
                is_primary: i == 0,
            })
            .collect())
    }

    /// Find the `scrap` display index whose DXGI device name matches a GDI
    /// display name such as `\\.\DISPLAY10`.
    ///
    /// RustDesk's Windows fallback preserves display identity by creating the
    /// fallback capturer for the same display object. The public `scrap`
    /// wrapper does not expose that name, so we query the Windows DXGI layer
    /// directly and reuse the same enumeration order as `Display::all()`.
    #[cfg(target_os = "windows")]
    pub fn windows_display_index_for_device_name(device_name: &str) -> Result<Option<usize>> {
        let displays = scrap::dxgi::Displays::new().context("failed to enumerate dxgi displays")?;
        for (index, display) in displays.enumerate() {
            let name = String::from_utf16_lossy(display.name());
            if name.eq_ignore_ascii_case(device_name) {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    pub fn display_index(&self) -> usize {
        self.display_index
    }
}

/// Information about an available display.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

impl std::fmt::Display for DisplayInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Display {}: {}x{}{}",
            self.index,
            self.width,
            self.height,
            if self.is_primary { " (primary)" } else { "" }
        )
    }
}

impl FrameCapture for ScrapCapture {
    fn capture(&mut self) -> Result<Option<Frame>> {
        #[cfg(target_os = "linux")]
        if self.x11_fallback_active {
            if let Some(ref fallback) = self.x11_fallback {
                return fallback.capture().map(Some);
            }
        }

        let capturer = self.capturer.as_mut().context("capturer not initialized")?;
        match capturer.frame() {
            Ok(frame) => {
                // scrap returns BGRA on all platforms, but stride may differ
                // from width * 4 due to padding. Copy row by row.
                if self.height == 0 || self.width == 0 {
                    return Ok(None);
                }
                let stride = frame.len() / self.height as usize;
                let bpp = 4;
                let expected_stride = self.width as usize * bpp;

                let data = if stride == expected_stride {
                    frame.to_vec()
                } else {
                    let mut data = Vec::with_capacity(expected_stride * self.height as usize);
                    for y in 0..self.height as usize {
                        let row_start = y * stride;
                        data.extend_from_slice(&frame[row_start..row_start + expected_stride]);
                    }
                    data
                };

                #[cfg(target_os = "linux")]
                if frame_is_mostly_black(&data) {
                    if let Some(ref fallback) = self.x11_fallback {
                        match fallback.capture() {
                            Ok(fallback_frame) => {
                                if !frame_is_mostly_black(&fallback_frame.data) {
                                    self.x11_fallback_active = true;
                                    tracing::warn!(
                                        "scrap returned a black frame; switching to X11 GetImage fallback"
                                    );
                                    return Ok(Some(fallback_frame));
                                }
                            }
                            Err(e) => tracing::debug!("X11 GetImage fallback capture failed: {e}"),
                        }
                    }
                }

                Ok(Some(Frame {
                    width: self.width,
                    height: self.height,
                    format: PixelFormat::Bgra8,
                    data,
                    timestamp: Instant::now(),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn reset(&mut self) -> Result<()> {
        self.reset()
    }
}

#[cfg(target_os = "linux")]
fn frame_is_mostly_black(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }

    let pixels = data.len() / 4;
    let step = (pixels / 4096).max(1);
    let mut sampled = 0usize;
    let mut black = 0usize;

    for pixel in (0..pixels).step_by(step) {
        let offset = pixel * 4;
        let b = data[offset] as u16;
        let g = data[offset + 1] as u16;
        let r = data[offset + 2] as u16;
        sampled += 1;
        if r < 8 && g < 8 && b < 8 {
            black += 1;
        }
    }

    sampled > 0 && black * 1000 / sampled >= 995
}

#[cfg(target_os = "linux")]
struct X11GetImageCapture {
    xlib: x11_dl::xlib::Xlib,
    display: *mut x11_dl::xlib::Display,
    drawable: std::os::raw::c_ulong,
    width: u32,
    height: u32,
    logged_first_frame: std::cell::Cell<bool>,
}

#[cfg(target_os = "linux")]
impl X11GetImageCapture {
    fn new(width: u32, height: u32) -> Result<Self> {
        let xlib = x11_dl::xlib::Xlib::open().map_err(|e| anyhow::anyhow!("{e}"))?;
        let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
        if display.is_null() {
            anyhow::bail!("open X11 display");
        }
        let screen = unsafe { (xlib.XDefaultScreen)(display) };
        let root = unsafe { (xlib.XRootWindow)(display, screen) };
        let screen_width = unsafe { (xlib.XDisplayWidth)(display, screen) }.max(1) as u32;
        let screen_height = unsafe { (xlib.XDisplayHeight)(display, screen) }.max(1) as u32;
        let width = width.min(screen_width);
        let height = height.min(screen_height);
        let drawable = find_desktop_drawable(&xlib, display, root, width, height).unwrap_or(root);
        if drawable != root {
            tracing::info!(
                root = format_args!("0x{root:x}"),
                drawable = format_args!("0x{drawable:x}"),
                "X11 GetImage using desktop drawable"
            );
        }
        Ok(Self {
            xlib,
            display,
            drawable,
            width,
            height,
            logged_first_frame: std::cell::Cell::new(false),
        })
    }

    fn capture(&self) -> Result<Frame> {
        let expected = self.width as usize * self.height as usize * 4;

        let image = unsafe {
            (self.xlib.XGetImage)(
                self.display,
                self.drawable,
                0,
                0,
                self.width,
                self.height,
                !0,
                x11_dl::xlib::ZPixmap,
            )
        };
        if image.is_null() {
            anyhow::bail!("XGetImage returned null");
        }

        let data = unsafe {
            let image_ref = &*image;
            let bytes_per_line = image_ref.bytes_per_line.max(0) as usize;
            let bits_per_pixel = image_ref.bits_per_pixel;
            let raw_len = bytes_per_line
                .checked_mul(self.height as usize)
                .ok_or_else(|| anyhow::anyhow!("XGetImage size overflow"))?;
            let raw = std::slice::from_raw_parts(image_ref.data as *const u8, raw_len);

            let mut out = Vec::with_capacity(expected);
            if bits_per_pixel == 32 {
                let row_bytes = self.width as usize * 4;
                for y in 0..self.height as usize {
                    let start = y * bytes_per_line;
                    out.extend_from_slice(&raw[start..start + row_bytes]);
                }
            } else if bits_per_pixel == 24 {
                let row_bytes = self.width as usize * 3;
                for y in 0..self.height as usize {
                    let start = y * bytes_per_line;
                    for chunk in raw[start..start + row_bytes].chunks_exact(3) {
                        out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0]);
                    }
                }
            } else {
                let _ = (self.xlib.XDestroyImage)(image);
                anyhow::bail!("unsupported XGetImage bpp: {bits_per_pixel}");
            }

            let _ = (self.xlib.XDestroyImage)(image);
            out
        };

        if !self.logged_first_frame.replace(true) {
            let (black_pct, mean_r, mean_g, mean_b) = frame_sample_stats(&data);
            tracing::info!(
                black_pct,
                mean_r,
                mean_g,
                mean_b,
                first_4 = ?&data[..data.len().min(4)],
                "X11 GetImage first frame stats"
            );
        }

        Ok(Frame {
            width: self.width,
            height: self.height,
            format: PixelFormat::Bgra8,
            data,
            timestamp: Instant::now(),
        })
    }
}

#[cfg(target_os = "linux")]
fn frame_sample_stats(data: &[u8]) -> (u32, u32, u32, u32) {
    if data.len() < 4 {
        return (0, 0, 0, 0);
    }
    let pixels = data.len() / 4;
    let step = (pixels / 4096).max(1);
    let mut sampled = 0u32;
    let mut black = 0u32;
    let mut sum_r = 0u64;
    let mut sum_g = 0u64;
    let mut sum_b = 0u64;
    for pixel in (0..pixels).step_by(step) {
        let offset = pixel * 4;
        let b = data[offset] as u32;
        let g = data[offset + 1] as u32;
        let r = data[offset + 2] as u32;
        sampled += 1;
        sum_r += r as u64;
        sum_g += g as u64;
        sum_b += b as u64;
        if r < 8 && g < 8 && b < 8 {
            black += 1;
        }
    }
    if sampled == 0 {
        return (0, 0, 0, 0);
    }
    (
        black * 100 / sampled,
        (sum_r / sampled as u64) as u32,
        (sum_g / sampled as u64) as u32,
        (sum_b / sampled as u64) as u32,
    )
}

#[cfg(target_os = "linux")]
fn find_desktop_drawable(
    xlib: &x11_dl::xlib::Xlib,
    display: *mut x11_dl::xlib::Display,
    root: std::os::raw::c_ulong,
    width: u32,
    height: u32,
) -> Option<std::os::raw::c_ulong> {
    use std::ffi::CString;
    use std::os::raw::{c_int, c_uchar, c_ulong};

    let prop_name = CString::new("_NET_CLIENT_LIST_STACKING").ok()?;
    let prop = unsafe { (xlib.XInternAtom)(display, prop_name.as_ptr(), 1) };
    if prop == 0 {
        return None;
    }

    let mut actual_type: c_ulong = 0;
    let mut actual_format: c_int = 0;
    let mut nitems: c_ulong = 0;
    let mut bytes_after: c_ulong = 0;
    let mut data: *mut c_uchar = std::ptr::null_mut();

    let status = unsafe {
        (xlib.XGetWindowProperty)(
            display,
            root,
            prop,
            0,
            4096,
            0,
            x11_dl::xlib::XA_WINDOW,
            &mut actual_type,
            &mut actual_format,
            &mut nitems,
            &mut bytes_after,
            &mut data,
        )
    };
    if status != x11_dl::xlib::Success as i32 || data.is_null() || actual_format != 32 {
        if !data.is_null() {
            unsafe {
                let _ = (xlib.XFree)(data.cast());
            }
        }
        return None;
    }

    let windows = unsafe { std::slice::from_raw_parts(data as *const c_ulong, nitems as usize) };
    let mut best = None;
    let mut best_area = 0i64;
    for &window in windows {
        let mut attrs = std::mem::MaybeUninit::<x11_dl::xlib::XWindowAttributes>::uninit();
        let ok = unsafe { (xlib.XGetWindowAttributes)(display, window, attrs.as_mut_ptr()) };
        if ok == 0 {
            continue;
        }
        let attrs = unsafe { attrs.assume_init() };
        if attrs.class != x11_dl::xlib::InputOutput
            || attrs.map_state != x11_dl::xlib::IsViewable
            || attrs.width <= 0
            || attrs.height <= 0
        {
            continue;
        }
        let covers_origin = attrs.x <= 0 && attrs.y <= 0;
        let large_enough = attrs.width as u32 >= width.saturating_mul(3) / 4
            && attrs.height as u32 >= height.saturating_mul(3) / 4;
        if !covers_origin || !large_enough {
            continue;
        }
        let area = attrs.width as i64 * attrs.height as i64;
        if area > best_area {
            best = Some(window);
            best_area = area;
        }
    }

    unsafe {
        let _ = (xlib.XFree)(data.cast());
    }
    best
}

#[cfg(target_os = "linux")]
impl Drop for X11GetImageCapture {
    fn drop(&mut self) {
        if !self.display.is_null() {
            unsafe {
                let _ = (self.xlib.XCloseDisplay)(self.display);
            }
            self.display = std::ptr::null_mut();
        }
    }
}
