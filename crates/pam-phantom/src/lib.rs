use pamsm::{pam_module, Pam, PamError, PamFlags, PamLibExt, PamServiceModule};
use std::fs;

const TICKET_PATH: &str = "/run/phantom/auth";

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

        let ticket = match fs::read_to_string(TICKET_PATH) {
            Ok(s) => s.trim().to_owned(),
            Err(e) => {
                eprintln!("pam_phantom: no ticket at {TICKET_PATH}: {e}");
                return PamError::AUTH_ERR;
            }
        };

        if ticket == want_user {
            eprintln!("pam_phantom: authenticated {want_user} via ticket");
            PamError::SUCCESS
        } else {
            eprintln!("pam_phantom: ticket mismatch (want={want_user} got={ticket})");
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
