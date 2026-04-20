use pamsm::{pam_module, Pam, PamError, PamFlags, PamLibExt, PamServiceModule};
use std::fs;
use std::time::{Duration, SystemTime};

const TICKET_PATH: &str = "/run/phantom/auth";
/// How long a ticket is considered fresh after phantom-server wrote it.
/// If nothing consumes it within this window it's ignored (prevents a
/// stale ticket from auto-logging in the wrong user).
const TICKET_MAX_AGE: Duration = Duration::from_secs(10);

struct PhantomPam;

impl PamServiceModule for PhantomPam {
    fn authenticate(pamh: Pam, _flags: PamFlags, _args: Vec<String>) -> PamError {
        let want_user = match pamh.get_user(None) {
            Ok(Some(u)) => u.to_string_lossy().into_owned(),
            _ => {
                eprintln!("pam_phantom: no PAM_USER");
                return PamError::USER_UNKNOWN;
            }
        };

        // TTL check — reject tickets older than TICKET_MAX_AGE.
        if let Ok(meta) = fs::metadata(TICKET_PATH) {
            if let Ok(modified) = meta.modified() {
                if let Ok(age) = SystemTime::now().duration_since(modified) {
                    if age > TICKET_MAX_AGE {
                        eprintln!("pam_phantom: ticket stale ({}s old), ignoring", age.as_secs());
                        let _ = fs::remove_file(TICKET_PATH);
                        return PamError::AUTH_ERR;
                    }
                }
            }
        }

        let ticket = match fs::read_to_string(TICKET_PATH) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("pam_phantom: no ticket at {TICKET_PATH}: {e}");
                return PamError::AUTH_ERR;
            }
        };
        // Payload is `user` or `user:password`; only the user prefix matters
        // to us (phantom-server writes password too for the Windows CP).
        let ticket_user = ticket
            .lines()
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .trim();

        if ticket_user == want_user {
            eprintln!("pam_phantom: authenticated {want_user} via ticket");
            // Single-use: burn the ticket so a stale copy can't auto-log
            // someone in later.
            let _ = fs::remove_file(TICKET_PATH);
            PamError::SUCCESS
        } else {
            eprintln!("pam_phantom: ticket mismatch (want={want_user} got={ticket_user})");
            // Mismatch leaves the ticket in place — the intended user may
            // still come along within the TTL window.
            PamError::AUTH_ERR
        }
    }

    fn setcred(_pamh: Pam, _flags: PamFlags, _args: Vec<String>) -> PamError {
        PamError::SUCCESS
    }

    fn acct_mgmt(_pamh: Pam, _flags: PamFlags, _args: Vec<String>) -> PamError {
        PamError::SUCCESS
    }
}

pam_module!(PhantomPam);
