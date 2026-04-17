//! Modern CCD (Connecting and Configuring Displays) API wrappers.
//!
//! Makes the VDD the **primary** display via `QueryDisplayConfig` +
//! `SetDisplayConfig` (with `SDC_VIRTUAL_MODE_AWARE` — required for IDD drivers).
//! Required because the legacy `ChangeDisplaySettingsExW(CDS_SET_PRIMARY)` API
//! returns `DISP_CHANGE_FAILED` on Windows 11 24H2+ with IDD-based virtual
//! displays (VDD issue #471).
//!
//! # Safety design — NEVER detach physical displays
//!
//! Earlier versions cleared `PATH_ACTIVE` on non-VDD paths so VDD was the
//! ONLY active display. This worked, but when combined with uninstall (which
//! force-kills the agent, bypassing topology restore, then removes VDD), it
//! left Windows with "last active topology = {VDD-only}" and no VDD driver on
//! next boot → boot hang. Two Win10/Win11 VMs got bricked this way.
//!
//! Sunshine (`libdisplaydevice`) never detaches physical paths — VDD is added
//! as an *extension* display, marked primary via source-mode position (0,0).
//! We follow the same pattern. Uninstall can never brick because physical
//! displays stay active throughout.
//!
//! Runtime-only (no `SDC_SAVE_TO_DATABASE`) — reboot reverts to defaults.
#![cfg(target_os = "windows")]

use anyhow::{bail, Context, Result};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::ERROR_SUCCESS;

/// Flag inside `DISPLAYCONFIG_PATH_INFO.flags`. Not exposed by windows-rs 0.58.
const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;

#[derive(Clone)]
pub struct Topology {
    pub paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    pub modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

/// Query all active display paths + modes (virtual-mode-aware for IDD support).
pub fn query_active_config() -> Result<Topology> {
    unsafe {
        let flags = QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE;
        let mut path_count: u32 = 0;
        let mut mode_count: u32 = 0;
        let r = GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count);
        if r != ERROR_SUCCESS {
            bail!("GetDisplayConfigBufferSizes failed: {:?}", r);
        }
        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> =
            vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> =
            vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let r = QueryDisplayConfig(
            flags,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        );
        if r != ERROR_SUCCESS {
            bail!("QueryDisplayConfig failed: {:?}", r);
        }
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        Ok(Topology { paths, modes })
    }
}

/// Get the GDI device name (e.g. `\\.\DISPLAY6`) for a path's source.
fn gdi_name_for_path(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String> {
    unsafe {
        let mut info = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        info.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        info.header.adapterId = path.sourceInfo.adapterId;
        info.header.id = path.sourceInfo.id;
        let r = DisplayConfigGetDeviceInfo(&mut info.header as *mut _);
        if r != ERROR_SUCCESS.0 as i32 {
            bail!("DisplayConfigGetDeviceInfo failed: {r}");
        }
        let end = info
            .viewGdiDeviceName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.viewGdiDeviceName.len());
        Ok(String::from_utf16_lossy(&info.viewGdiDeviceName[..end]))
    }
}

/// Locate the path whose GDI source name matches `vdd_gdi_name`.
pub fn find_vdd_path_idx(topo: &Topology, vdd_gdi_name: &str) -> Option<usize> {
    for (idx, path) in topo.paths.iter().enumerate() {
        if (path.flags & DISPLAYCONFIG_PATH_ACTIVE) == 0 {
            continue;
        }
        match gdi_name_for_path(path) {
            Ok(name) if name == vdd_gdi_name => return Some(idx),
            _ => continue,
        }
    }
    None
}

/// Extract the source-mode index from a path's bitfield. In virtual-mode-aware
/// mode the `sourceInfo.Anonymous` field is `{cloneGroupId:16, sourceModeInfoIdx:16}`.
fn source_mode_idx(path: &DISPLAYCONFIG_PATH_INFO) -> usize {
    unsafe {
        let bf = path.sourceInfo.Anonymous.modeInfoIdx;
        ((bf >> 16) & 0xFFFF) as usize
    }
}

/// Fast-path: is VDD already the primary (at (0,0))? Skips the ~100-200ms
/// SetDisplayConfig call when no change is needed.
pub fn is_vdd_primary(vdd_gdi_name: &str) -> bool {
    let topo = match query_active_config() {
        Ok(t) => t,
        Err(_) => return false,
    };
    let vdd_idx = match find_vdd_path_idx(&topo, vdd_gdi_name) {
        Some(i) => i,
        None => return false,
    };
    let src_idx = source_mode_idx(&topo.paths[vdd_idx]);
    if src_idx >= topo.modes.len()
        || topo.modes[src_idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
    {
        return false;
    }
    unsafe {
        let p = topo.modes[src_idx].Anonymous.sourceMode.position;
        p.x == 0 && p.y == 0
    }
}

/// Make VDD the primary display by shifting all source-mode positions so VDD
/// lands at (0,0). Physical displays stay active — they just move to positive
/// coordinates (Windows treats the monitor at (0,0) as primary).
///
/// Returns the original topology so the caller can restore it on shutdown.
pub fn set_vdd_primary(vdd_gdi_name: &str) -> Result<Topology> {
    let current = query_active_config()?;
    let vdd_idx = find_vdd_path_idx(&current, vdd_gdi_name)
        .with_context(|| format!("VDD path not found: {vdd_gdi_name}"))?;

    // Find VDD's source mode position — that's the shift vector.
    let src_idx = source_mode_idx(&current.paths[vdd_idx]);
    let (shift_x, shift_y) = unsafe {
        if src_idx < current.modes.len()
            && current.modes[src_idx].infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
        {
            let p = current.modes[src_idx].Anonymous.sourceMode.position;
            (p.x, p.y)
        } else {
            bail!("VDD source mode not found at idx {src_idx}");
        }
    };
    if shift_x == 0 && shift_y == 0 {
        // Already primary — no-op.
        crate::service_win::svc_log(&format!(
            "CCD: VDD {vdd_gdi_name} already at (0,0), skipping"
        ));
        return Ok(current);
    }

    let paths = current.paths.clone();
    let mut modes = current.modes.clone();
    // Shift every source mode by (-shift_x, -shift_y) so VDD lands at (0,0).
    for m in modes.iter_mut() {
        unsafe {
            if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                m.Anonymous.sourceMode.position.x -= shift_x;
                m.Anonymous.sourceMode.position.y -= shift_y;
            }
        }
    }
    crate::service_win::svc_log(&format!(
        "CCD: shifting all sources by ({},{}); VDD → (0,0); physical paths STAY ACTIVE",
        -shift_x, -shift_y
    ));

    apply(&paths, &modes).context("SetDisplayConfig (set_vdd_primary)")?;
    Ok(current)
}

/// Apply a previously saved topology. Used to restore on graceful shutdown.
pub fn restore(topo: &Topology) -> Result<()> {
    apply(&topo.paths, &topo.modes).context("SetDisplayConfig (restore)")
}

fn apply(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> Result<()> {
    unsafe {
        // No SDC_SAVE_TO_DATABASE — stay runtime-only so reboot reverts.
        let flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_VIRTUAL_MODE_AWARE;
        let r = SetDisplayConfig(Some(paths), Some(modes), flags);
        if r != ERROR_SUCCESS.0 as i32 {
            // Retry with SDC_ALLOW_CHANGES as a permissive fallback.
            let r2 = SetDisplayConfig(
                Some(paths),
                Some(modes),
                SDC_APPLY
                    | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                    | SDC_ALLOW_CHANGES
                    | SDC_VIRTUAL_MODE_AWARE,
            );
            if r2 != ERROR_SUCCESS.0 as i32 {
                bail!("SetDisplayConfig failed: primary={r} fallback={r2}");
            }
        }
        Ok(())
    }
}
