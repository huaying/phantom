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
//! ONLY active display. This worked, but when uninstall or service relaunch
//! happened while VDD-only topology was current, Windows could persist a
//! topology that no longer had a valid VDD driver on next boot. Two Win10/Win11
//! VMs got bricked this way.
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
const DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE: u32 = 0x0000_0008;
const DISPLAYCONFIG_PATH_MODE_IDX_INVALID: u32 = 0xffff_ffff;

#[derive(Clone)]
pub struct Topology {
    pub paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    pub modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

/// Query all active display paths + modes (virtual-mode-aware for IDD support).
pub fn query_active_config() -> Result<Topology> {
    query_config(QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE)
}

fn query_all_config() -> Result<Topology> {
    query_config(QDC_ALL_PATHS | QDC_VIRTUAL_MODE_AWARE)
}

fn query_config(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<Topology> {
    unsafe {
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
        let raw = path.sourceInfo.Anonymous.modeInfoIdx;
        if (path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE) != 0 {
            ((raw >> 16) & 0xFFFF) as usize
        } else {
            raw as usize
        }
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

/// Return the active CCD source rectangle for a GDI display name.
///
/// Prefer this over `EnumDisplaySettingsW` during Winlogon/Default transitions:
/// CCD is the topology we just applied with `SetDisplayConfig`, while legacy
/// GDI settings can lag behind and report the display's previous position.
pub fn active_source_rect(gdi_name: &str) -> Option<(i32, i32, u32, u32)> {
    let topo = query_active_config().ok()?;
    source_rect_from_topology(&topo, gdi_name)
}

pub fn source_rect_from_topology(topo: &Topology, gdi_name: &str) -> Option<(i32, i32, u32, u32)> {
    let path_idx = find_vdd_path_idx(topo, gdi_name)?;
    let src_idx = source_mode_idx(&topo.paths[path_idx]);
    if src_idx >= topo.modes.len()
        || topo.modes[src_idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
    {
        return None;
    }
    unsafe {
        let src = topo.modes[src_idx].Anonymous.sourceMode;
        Some((src.position.x, src.position.y, src.width, src.height))
    }
}

/// Ensure the VDD path is active without intentionally making it primary.
/// This lets DXGI enumerate the virtual output before Tier 1 capture starts.
pub fn ensure_vdd_active(vdd_gdi_name: &str) -> Result<Topology> {
    let current = query_active_config()?;
    if find_vdd_path_idx(&current, vdd_gdi_name).is_some() {
        return Ok(current);
    }

    crate::service_win::svc_log(&format!(
        "CCD: VDD {vdd_gdi_name} not active; enabling as extension"
    ));
    activate_vdd_extension_path(vdd_gdi_name).context("SetDisplayConfig (ensure VDD active)")?;
    let active = query_active_config().context("QueryDisplayConfig after VDD activation")?;
    find_vdd_path_idx(&active, vdd_gdi_name)
        .with_context(|| format!("VDD path not found after activation: {vdd_gdi_name}"))?;
    Ok(active)
}

/// Human-readable active topology summary for Windows capture diagnostics.
pub fn active_config_summary() -> Result<Vec<String>> {
    let topo = query_active_config()?;
    Ok(active_config_summary_from_topology(&topo))
}

fn active_config_summary_from_topology(topo: &Topology) -> Vec<String> {
    let mut lines = Vec::with_capacity(topo.paths.len());
    for (idx, path) in topo.paths.iter().enumerate() {
        let active = (path.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0;
        let flags = path.flags;
        let raw_source_mode = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
        let source_name = gdi_name_for_path(path).unwrap_or_else(|e| format!("<source-name: {e}>"));
        let src_idx = source_mode_idx(path);
        let source_mode = if src_idx < topo.modes.len()
            && topo.modes[src_idx].infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
        {
            unsafe {
                let src = topo.modes[src_idx].Anonymous.sourceMode;
                format!(
                    "{}x{} pos=({},{})",
                    src.width, src.height, src.position.x, src.position.y
                )
            }
        } else {
            format!("source-mode-missing idx={src_idx}")
        };
        lines.push(format!(
            "path[{idx}] active={active} flags=0x{flags:X} raw_source_mode=0x{raw_source_mode:X} source={source_name} {source_mode}"
        ));
    }
    lines
}

/// Make VDD the primary display by shifting all source-mode positions so VDD
/// lands at (0,0). Physical displays stay active — they just move to positive
/// coordinates (Windows treats the monitor at (0,0) as primary).
///
/// Returns the observed topology for diagnostics. Callers intentionally do not
/// restore topology on agent shutdown because the service relaunches agents
/// during Winlogon/Default transitions; restoring there causes resolution churn.
pub fn set_vdd_primary(vdd_gdi_name: &str) -> Result<Topology> {
    let mut current = query_active_config()?;
    let vdd_idx = match find_vdd_path_idx(&current, vdd_gdi_name) {
        Some(idx) => idx,
        None => {
            crate::service_win::svc_log(&format!(
                "CCD: VDD {vdd_gdi_name} not active; activating supplied extend path"
            ));
            activate_vdd_extension_path(vdd_gdi_name)
                .context("SetDisplayConfig (activate VDD extension path)")?;
            current =
                query_active_config().context("QueryDisplayConfig after VDD path activation")?;
            find_vdd_path_idx(&current, vdd_gdi_name)
                .with_context(|| format!("VDD path not found after activation: {vdd_gdi_name}"))?
        }
    };

    let src_idx = source_mode_idx(&current.paths[vdd_idx]);
    if src_idx >= current.modes.len()
        || current.modes[src_idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
    {
        bail!("VDD source mode not found at idx {src_idx}");
    }

    let paths = current.paths.clone();
    let mut modes = current.modes.clone();
    let vdd_width = unsafe { modes[src_idx].Anonymous.sourceMode.width.max(1) };
    let mut next_x = vdd_width as i32;
    let mut changed = false;
    let mut moved_sources = 0usize;

    for (idx, m) in modes.iter_mut().enumerate() {
        unsafe {
            if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                let source = &mut m.Anonymous.sourceMode;
                let (target_x, target_y) = if idx == src_idx {
                    (0, 0)
                } else {
                    let x = next_x;
                    next_x += source.width.max(1) as i32;
                    moved_sources += 1;
                    (x, 0)
                };
                if source.position.x != target_x || source.position.y != target_y {
                    changed = true;
                    source.position.x = target_x;
                    source.position.y = target_y;
                }
            }
        }
    }

    if !changed {
        crate::service_win::svc_log(&format!(
            "CCD: VDD {vdd_gdi_name} primary layout already stable; {moved_sources} other source(s)"
        ));
        for line in active_config_summary_from_topology(&current) {
            crate::service_win::svc_log(&format!("CCD stable primary: {line}"));
        }
        return Ok(current);
    }

    crate::service_win::svc_log(&format!(
        "CCD: applying VDD primary layout; VDD -> (0,0), {moved_sources} other source(s) moved right"
    ));

    apply(&paths, &modes).context("SetDisplayConfig (set_vdd_primary)")?;
    let observed = query_active_config().context("QueryDisplayConfig after set_vdd_primary")?;
    for line in active_config_summary_from_topology(&observed) {
        crate::service_win::svc_log(&format!("CCD after primary: {line}"));
    }
    Ok(observed)
}

fn apply(paths: &[DISPLAYCONFIG_PATH_INFO], modes: &[DISPLAYCONFIG_MODE_INFO]) -> Result<()> {
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

fn activate_vdd_extension_path(vdd_gdi_name: &str) -> Result<()> {
    let all = query_all_config()?;
    let mut selected: Vec<DISPLAYCONFIG_PATH_INFO> = Vec::new();
    let mut seen = Vec::new();
    let mut inactive_vdd: Option<DISPLAYCONFIG_PATH_INFO> = None;
    let mut active_count = 0usize;
    let mut available_count = 0usize;
    let mut vdd_candidates = Vec::new();

    for (idx, path) in all.paths.iter().enumerate() {
        let active = (path.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0;
        let source_name = gdi_name_for_path(path).unwrap_or_else(|e| format!("<source-name: {e}>"));
        let target_available = path.targetInfo.targetAvailable.as_bool();
        if active {
            active_count += 1;
        }
        if target_available {
            available_count += 1;
        }
        if source_name == vdd_gdi_name {
            vdd_candidates.push(format!(
                "path[{idx}] active={active} target_available={target_available}"
            ));
        }

        if active {
            push_supplied_path(&mut selected, &mut seen, *path);
        } else if source_name == vdd_gdi_name
            && inactive_vdd.as_ref().is_none_or(|_| target_available)
        {
            inactive_vdd = Some(*path);
        }
    }

    crate::service_win::svc_log(&format!(
        "CCD all paths: total={} active={} target_available={} vdd_candidates={}",
        all.paths.len(),
        active_count,
        available_count,
        vdd_candidates.join("; ")
    ));

    let mut vdd_path = inactive_vdd
        .with_context(|| format!("VDD path absent from QDC_ALL_PATHS: {vdd_gdi_name}"))?;
    vdd_path.flags |= DISPLAYCONFIG_PATH_ACTIVE;
    invalidate_path_modes(&mut vdd_path);
    // Keep existing active paths first so the activation step behaves like an
    // extend operation. `set_vdd_primary` can reorder source-mode positions
    // after DXGI/NVENC has successfully bound the VDD output.
    selected.push(vdd_path);

    if selected.is_empty() {
        bail!("no display paths selected for VDD activation");
    }

    unsafe {
        let apply = SetDisplayConfig(
            Some(&selected),
            None,
            SDC_APPLY
                | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                | SDC_ALLOW_CHANGES
                | SDC_VIRTUAL_MODE_AWARE,
        );
        if apply == ERROR_SUCCESS.0 as i32 {
            crate::service_win::svc_log(&format!(
                "CCD: supplied VDD topology applied with {} path(s)",
                selected.len()
            ));
            return Ok(());
        }

        let legacy_apply = SetDisplayConfig(
            Some(&selected),
            None,
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
        );
        if legacy_apply != ERROR_SUCCESS.0 as i32 {
            bail!(
                "SetDisplayConfig supplied VDD activation failed: apply={apply} legacy_apply={legacy_apply}"
            );
        }
        crate::service_win::svc_log(&format!(
            "CCD: supplied VDD topology applied without virtual-mode flag with {} path(s)",
            selected.len()
        ));
        Ok(())
    }
}

fn push_supplied_path(
    selected: &mut Vec<DISPLAYCONFIG_PATH_INFO>,
    seen: &mut Vec<((i32, u32, u32), (i32, u32, u32))>,
    mut path: DISPLAYCONFIG_PATH_INFO,
) {
    let key = path_key(&path);
    if seen.contains(&key) {
        return;
    }
    path.flags |= DISPLAYCONFIG_PATH_ACTIVE;
    invalidate_path_modes(&mut path);
    selected.push(path);
    seen.push(key);
}

fn path_key(path: &DISPLAYCONFIG_PATH_INFO) -> ((i32, u32, u32), (i32, u32, u32)) {
    (
        (
            path.sourceInfo.adapterId.HighPart,
            path.sourceInfo.adapterId.LowPart,
            path.sourceInfo.id,
        ),
        (
            path.targetInfo.adapterId.HighPart,
            path.targetInfo.adapterId.LowPart,
            path.targetInfo.id,
        ),
    )
}

fn invalidate_path_modes(path: &mut DISPLAYCONFIG_PATH_INFO) {
    path.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
}
