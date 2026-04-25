//! GDI-based screen capture for Windows Session 0 / lock screen.
//!
//! When running as a Windows Service in Session 0, DXGI Desktop Duplication
//! is not available (it requires an interactive desktop session). This module
//! provides a fallback using GDI's BitBlt to capture the screen.
//!
//! Performance: ~15-30 FPS at 1080p (slower than DXGI's ~60 FPS zero-copy),
//! but sufficient for lock screen / login screen viewing.
//!
//! To capture the Winlogon Secure Desktop (lock screen), we use
//! OpenInputDesktop + SetThreadDesktop to switch to whichever desktop is active.

use anyhow::Result;
use phantom_core::capture::FrameCapture;
use phantom_core::frame::{Frame, PixelFormat};
use std::time::Instant;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    RedrawWindow, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW, SRCCOPY,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_CREATEWINDOW, DESKTOP_READOBJECTS,
    DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, UOI_NAME,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetDesktopWindow, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
};

/// Switch the current thread to whichever desktop is receiving input.
/// Used by both GdiCapture and the agent loop to follow desktop switches
/// (e.g., lock screen / user desktop transitions).
///
/// Returns true if the switch succeeded, false otherwise.
pub fn switch_to_input_desktop() -> bool {
    unsafe {
        // SetThreadDesktop requires DESKTOP_CREATEWINDOW access. Request the
        // read/write/switch rights explicitly instead of relying on GENERIC_*
        // mapping, which is opaque and has behaved differently across images.
        let hdesk = match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, input_desktop_access())
        {
            Ok(d) => d,
            Err(_) => return false,
        };
        let ok = SetThreadDesktop(hdesk).is_ok();
        let _ = CloseDesktop(hdesk);
        ok
    }
}

/// Get the name of the currently active input desktop (e.g., "Default",
/// "Winlogon", "Screen-saver"). Used to detect desktop transitions
/// (lock/unlock/UAC prompts) so the agent can reset DXGI duplication —
/// duplication objects are scoped to the desktop they were created on and
/// keep returning stale frames from the wrong desktop after a switch.
pub fn current_input_desktop_name() -> Option<String> {
    unsafe {
        let hdesk = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS).ok()?;
        let mut buf = [0u16; 256];
        let mut needed: u32 = 0;
        let ok = GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(hdesk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut _),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
        .is_ok();
        let _ = CloseDesktop(hdesk);
        if !ok {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

/// Ask Windows to repaint the interactive desktop.
///
/// DXGI/VDD can report a valid frame immediately after a mode/topology switch
/// while the virtual display still contains an all-black surface. A repaint
/// nudge is harmless when the desktop is already fresh and helps headless
/// VDD sessions produce a real first frame without requiring user input.
pub fn nudge_desktop_repaint() {
    unsafe {
        let hwnd = GetDesktopWindow();
        let _ = RedrawWindow(
            hwnd,
            None,
            None,
            RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN,
        );
    }
}

fn input_desktop_access() -> DESKTOP_ACCESS_FLAGS {
    DESKTOP_ACCESS_FLAGS(
        DESKTOP_CREATEWINDOW.0
            | DESKTOP_READOBJECTS.0
            | DESKTOP_WRITEOBJECTS.0
            | DESKTOP_SWITCHDESKTOP.0,
    )
}

/// GDI-based screen capture. Works in Session 0 (service context).
pub struct GdiCapture {
    width: u32,
    height: u32,
}

impl GdiCapture {
    pub fn new() -> Result<Self> {
        let (width, height) = unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN) as u32,
                GetSystemMetrics(SM_CYSCREEN) as u32,
            )
        };

        if width == 0 || height == 0 {
            anyhow::bail!(
                "GDI capture: screen dimensions are 0x0 (no desktop available in Session 0?)"
            );
        }

        tracing::info!(width, height, "GdiCapture initialized (Session 0 fallback)");
        Ok(Self { width, height })
    }

    /// Switch the current thread to whichever desktop is receiving input.
    /// Delegates to the module-level [`switch_to_input_desktop()`] function.
    fn switch_to_active_desktop(&self) -> bool {
        switch_to_input_desktop()
    }
}

impl FrameCapture for GdiCapture {
    fn capture(&mut self) -> Result<Option<Frame>> {
        // Try to switch to the active input desktop (Winlogon / user desktop).
        // If this fails, we'll still capture whatever desktop we're on.
        let _ = self.switch_to_active_desktop();

        unsafe {
            let hwnd = GetDesktopWindow();
            let hdc_screen = GetDC(hwnd);
            if hdc_screen.is_invalid() {
                anyhow::bail!("GetDC failed for desktop window");
            }

            let hdc_mem = CreateCompatibleDC(hdc_screen);
            if hdc_mem.is_invalid() {
                ReleaseDC(hwnd, hdc_screen);
                anyhow::bail!("CreateCompatibleDC failed");
            }

            let hbm = CreateCompatibleBitmap(hdc_screen, self.width as i32, self.height as i32);
            if hbm.is_invalid() {
                let _ = DeleteDC(hdc_mem);
                ReleaseDC(hwnd, hdc_screen);
                anyhow::bail!("CreateCompatibleBitmap failed");
            }

            let old_bm = SelectObject(hdc_mem, hbm);

            // BitBlt: copy screen → memory DC
            let result = BitBlt(
                hdc_mem,
                0,
                0,
                self.width as i32,
                self.height as i32,
                hdc_screen,
                0,
                0,
                SRCCOPY,
            );

            if result.is_err() {
                SelectObject(hdc_mem, old_bm);
                let _ = DeleteObject(hbm);
                let _ = DeleteDC(hdc_mem);
                ReleaseDC(hwnd, hdc_screen);
                anyhow::bail!("BitBlt failed");
            }

            // Read pixels from bitmap
            let bpp = 4u32;
            let data_size = (self.width * self.height * bpp) as usize;
            let mut data = vec![0u8; data_size];

            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: self.width as i32,
                    biHeight: -(self.height as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [Default::default()],
            };

            let lines = GetDIBits(
                hdc_mem,
                hbm,
                0,
                self.height,
                Some(data.as_mut_ptr() as *mut _),
                &mut bmi,
                DIB_RGB_COLORS,
            );

            // Cleanup GDI resources
            SelectObject(hdc_mem, old_bm);
            let _ = DeleteObject(hbm);
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(hwnd, hdc_screen);

            if lines == 0 {
                anyhow::bail!("GetDIBits failed (0 lines copied)");
            }

            Ok(Some(Frame {
                width: self.width,
                height: self.height,
                format: PixelFormat::Bgra8, // GDI with 32bpp returns BGRA
                data,
                timestamp: Instant::now(),
            }))
        }
    }

    fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn reset(&mut self) -> Result<()> {
        // Re-read screen dimensions in case resolution changed
        unsafe {
            let w = GetSystemMetrics(SM_CXSCREEN) as u32;
            let h = GetSystemMetrics(SM_CYSCREEN) as u32;
            if w > 0 && h > 0 {
                self.width = w;
                self.height = h;
            }
        }
        Ok(())
    }
}
