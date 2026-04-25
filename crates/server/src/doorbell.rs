//! Pure session-affinity decision used by the doorbell threads in
//! `main.rs` (Linux + Windows console mode) and `service_win.rs` (Windows
//! service mode). Splitting it out makes the policy unit-testable without
//! standing up a real transport.
//!
//! Policy:
//! - **Ghost id** (already in the kicked set) → reject.
//! - **Current id** (same client reconnecting) → accept, no ghost shuffle.
//! - **New id** → accept and demote whoever was current to the ghost set.
//! - **No id** (legacy client, no `ClientHello`) → accept, but don't mutate
//!   the tracked owner/ghost set. Anonymous or failed handshakes should not
//!   poison browser client affinity state.

use std::collections::VecDeque;

/// Cap on the bounded VecDeque. Sized for the "N forgotten browser tabs"
/// scenario — large enough to remember every recent ejection, small
/// enough to bound memory.
pub const GHOST_MAX: usize = 16;

/// Outcome the doorbell loop should take.
#[derive(Debug, PartialEq, Eq)]
pub enum DoorbellDecision {
    /// Accept the connection. Caller pushes it into the pending slot.
    Accept,
    /// Reject the connection — caller drops the transport handles so the
    /// client sees onclose.
    Reject,
}

/// Apply the affinity policy to one incoming connection.
///
/// `current` is the id of whoever owns the active session (Some) or the
/// session is idle (None). `ghosts` is the bounded LRU set of recently
/// kicked ids. Both are mutated in place when the decision changes the
/// active owner.
pub fn decide(
    incoming: Option<[u8; 16]>,
    current: &mut Option<[u8; 16]>,
    ghosts: &mut VecDeque<[u8; 16]>,
) -> DoorbellDecision {
    match incoming {
        Some(id) if ghosts.iter().any(|g| g == &id) => DoorbellDecision::Reject,
        Some(id) if *current == Some(id) => DoorbellDecision::Accept,
        Some(id) => {
            if let Some(old) = current.take() {
                push_ghost(ghosts, old);
            }
            *current = Some(id);
            DoorbellDecision::Accept
        }
        None => DoorbellDecision::Accept,
    }
}

fn push_ghost(ghosts: &mut VecDeque<[u8; 16]>, id: [u8; 16]) {
    ghosts.push_back(id);
    while ghosts.len() > GHOST_MAX {
        ghosts.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    #[test]
    fn first_client_accepted_and_tracked() {
        let mut current = None;
        let mut ghosts = VecDeque::new();
        assert_eq!(
            decide(Some(id(1)), &mut current, &mut ghosts),
            DoorbellDecision::Accept
        );
        assert_eq!(current, Some(id(1)));
        assert!(ghosts.is_empty(), "no one to ghost on first accept");
    }

    #[test]
    fn same_client_reconnect_is_accepted_without_ghost_shuffle() {
        let mut current = Some(id(1));
        let mut ghosts = VecDeque::new();
        assert_eq!(
            decide(Some(id(1)), &mut current, &mut ghosts),
            DoorbellDecision::Accept
        );
        assert_eq!(current, Some(id(1)));
        assert!(
            ghosts.is_empty(),
            "same id reconnect must not push self to ghost set"
        );
    }

    #[test]
    fn new_client_takes_over_and_demotes_old() {
        let mut current = Some(id(1));
        let mut ghosts = VecDeque::new();
        assert_eq!(
            decide(Some(id(2)), &mut current, &mut ghosts),
            DoorbellDecision::Accept
        );
        assert_eq!(current, Some(id(2)));
        assert_eq!(ghosts.len(), 1);
        assert_eq!(ghosts[0], id(1));
    }

    #[test]
    fn ghost_client_is_rejected() {
        let mut current = Some(id(2));
        let mut ghosts = VecDeque::from(vec![id(1)]);
        assert_eq!(
            decide(Some(id(1)), &mut current, &mut ghosts),
            DoorbellDecision::Reject
        );
        // State unchanged — id 2 still owns the session, id 1 still ghost.
        assert_eq!(current, Some(id(2)));
        assert_eq!(ghosts.len(), 1);
    }

    #[test]
    fn legacy_client_with_no_id_is_accepted_without_mutating_tracking() {
        let mut current = Some(id(1));
        let mut ghosts = VecDeque::new();
        assert_eq!(
            decide(None, &mut current, &mut ghosts),
            DoorbellDecision::Accept
        );
        assert_eq!(current, Some(id(1)));
        assert!(ghosts.is_empty());
    }

    #[test]
    fn ghost_set_caps_at_max_evicting_oldest() {
        let mut current;
        let mut ghosts = VecDeque::new();
        // Push GHOST_MAX + 5 unique ids through the takeover path.
        for i in 0..(GHOST_MAX + 5) as u8 {
            // Establish current
            current = Some(id(i));
            // Then immediately get displaced by next id
            decide(Some(id(i + 100)), &mut current, &mut ghosts);
        }
        assert_eq!(ghosts.len(), GHOST_MAX);
        // Oldest entries (lowest i) should have been evicted.
        assert!(!ghosts.contains(&id(0)));
        assert!(ghosts.contains(&id((GHOST_MAX + 4) as u8)));
    }

    #[test]
    fn ghost_then_fresh_id_accepted_takeover_path() {
        // Simulates: tab A connects, tab B takes over (A → ghost), tab A
        // reload gets fresh id Y, Y takes over from B.
        let mut current = None;
        let mut ghosts = VecDeque::new();
        decide(Some(id(1)), &mut current, &mut ghosts); // A connects
        decide(Some(id(2)), &mut current, &mut ghosts); // B takes over
                                                        // A's WS reconnect attempt with same id → rejected
        assert_eq!(
            decide(Some(id(1)), &mut current, &mut ghosts),
            DoorbellDecision::Reject
        );
        // A reloads → fresh id 3
        assert_eq!(
            decide(Some(id(3)), &mut current, &mut ghosts),
            DoorbellDecision::Accept
        );
        assert_eq!(current, Some(id(3)));
        // B (id 2) is now in ghost
        assert!(ghosts.contains(&id(2)));
    }
}
