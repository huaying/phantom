/// Clipboard sync logic shared between server and client.
/// Tracks last-known content to avoid echo loops.
#[derive(Default)]
pub struct ClipboardTracker {
    /// Content we last SET (from remote) — don't echo it back.
    last_set: String,
    /// Content we last READ (local) — detect local changes.
    last_read: String,
}

impl ClipboardTracker {
    pub fn new() -> Self {
        Self {
            last_set: String::new(),
            last_read: String::new(),
        }
    }

    /// Called when we receive clipboard content from the remote side.
    /// Returns true if this is new content (should be set on local clipboard).
    pub fn on_remote_update(&mut self, content: &str) -> bool {
        if content == self.last_set {
            return false; // already set this
        }
        self.last_set = content.to_string();
        self.last_read = content.to_string(); // prevent echo
        true
    }

    /// Called with current local clipboard content.
    /// Returns Some(content) if it changed and should be sent to remote.
    pub fn check_local_change(&mut self, current: &str) -> Option<String> {
        if current == self.last_read || current == self.last_set {
            return None;
        }
        self.last_read = current.to_string();
        Some(current.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_echo_loop() {
        let mut tracker = ClipboardTracker::new();

        // Remote sends "hello" → should set locally
        assert!(tracker.on_remote_update("hello"));

        // Local clipboard now reads "hello" → should NOT send back
        assert_eq!(tracker.check_local_change("hello"), None);

        // User copies "world" locally → should send to remote
        assert_eq!(tracker.check_local_change("world"), Some("world".into()));

        // Same content again → should not send
        assert_eq!(tracker.check_local_change("world"), None);
    }

    #[test]
    fn duplicate_remote_ignored() {
        let mut tracker = ClipboardTracker::new();
        assert!(tracker.on_remote_update("test"));
        assert!(!tracker.on_remote_update("test")); // duplicate
        assert!(tracker.on_remote_update("new")); // different
    }
}
