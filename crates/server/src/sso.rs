//! SSO agent: writes a side-channel auth ticket after a JWT is verified,
//! so the OS-level credential plugin (pam_phantom on Linux,
//! phantom_cp on Windows) can log the user straight into their desktop
//! session without a password prompt.
//!
//! Phase 2 — passwordless: phantom-server writes only the username to the
//! ticket. On Linux, pam_phantom has always ignored the password field.
//! On Windows, phantom_cp submits MSV1_0_S4U_LOGON to LSA (LogonUI has
//! SeTcbPrivilege, which is the precondition for caller-asserted logons).
//!
//! See docs/sso-plan.md.

/// Invoked by the WS auth path once a JWT has been verified. Writes the
/// ticket and (on Windows) kicks LogonUI so the Credential Provider
/// re-enumerates and auto-submits.
#[cfg(feature = "sso")]
pub fn on_jwt_verified(user: &str) {
    // Guard: refuse empty / whitespace-only usernames.
    let user = user.trim();
    if user.is_empty() {
        tracing::warn!("sso: empty user, skipping ticket write");
        return;
    }

    #[cfg(target_os = "linux")]
    {
        let dir = "/run/phantom";
        let path = "/run/phantom/auth";
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("sso: mkdir {dir}: {e}");
            return;
        }
        if let Err(e) = std::fs::write(path, user) {
            tracing::warn!("sso: write {path}: {e}");
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        tracing::info!(user, "sso: wrote {}", path);
    }

    #[cfg(target_os = "windows")]
    {
        let dir = r"C:\ProgramData\phantom";
        let path = r"C:\ProgramData\phantom\auth";
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("sso: mkdir {dir}: {e}");
            return;
        }
        if let Err(e) = std::fs::write(path, user) {
            tracing::warn!("sso: write {path}: {e}");
            return;
        }
        tracing::info!(user, "sso: wrote {}, kicking LogonUI", path);
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", "LogonUI.exe"])
            .output();
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = user;
    }
}

#[cfg(not(feature = "sso"))]
pub fn on_jwt_verified(_user: &str) {}
