//! SSO agent: writes a side-channel auth ticket after a JWT is verified,
//! so the OS-level credential plugin (pam_phantom on Linux,
//! phantom_cp on Windows) can log the user straight into their desktop
//! session without a password prompt.
//!
//! phantom-server doesn't link the plugin — the file on disk is the only
//! coupling. See docs/sso-plan.md.

#[cfg(feature = "sso")]
use std::sync::OnceLock;

#[cfg(feature = "sso")]
static SSO_PASSWORD: OnceLock<String> = OnceLock::new();

/// Called once at startup from main(). Stores the plaintext password
/// phantom-server will pair with every JWT `sub`. Phase-1 simplification;
/// Phase 2 replaces this with per-session passwordless S4U (Windows) /
/// delegated ticket (Linux).
#[cfg(feature = "sso")]
pub fn init(password: Option<String>) {
    if let Some(p) = password {
        let _ = SSO_PASSWORD.set(p);
        tracing::info!("sso: enabled (password loaded)");
    }
}

#[cfg(not(feature = "sso"))]
pub fn init(_password: Option<String>) {}

/// Invoked by the WS auth path once a JWT has been verified. If SSO is
/// configured, writes the auth file and (on Windows) kicks LogonUI so the
/// Credential Provider re-enumerates and auto-submits.
#[cfg(feature = "sso")]
pub fn on_jwt_verified(user: &str) {
    let pw = match SSO_PASSWORD.get() {
        Some(s) => s,
        None => return, // --sso-password-file not provided
    };
    let line = format!("{user}:{pw}");

    #[cfg(target_os = "linux")]
    {
        let dir = "/run/phantom";
        let path = "/run/phantom/auth";
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("sso: mkdir {dir}: {e}");
            return;
        }
        if let Err(e) = std::fs::write(path, &line) {
            tracing::warn!("sso: write {path}: {e}");
            return;
        }
        // 0600 is plenty — pam_phantom runs as root (inside PAM stack).
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
        if let Err(e) = std::fs::write(path, &line) {
            tracing::warn!("sso: write {path}: {e}");
            return;
        }
        tracing::info!(user, "sso: wrote {}, kicking LogonUI", path);
        // taskkill is a no-op if LogonUI isn't running (nobody at lock screen).
        // When it IS running, respawn re-enumerates CPs → ours auto-submits.
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", "LogonUI.exe"])
            .output();
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = line;
        let _ = user;
    }
}

#[cfg(not(feature = "sso"))]
pub fn on_jwt_verified(_user: &str) {}
