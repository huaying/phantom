//! Bind a dedicated audio socket to the main WSS session that issued its token.
//! The main sender owns the slot; registry entries cannot keep ended sessions alive.
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex, Weak};

type AudioSlot = Mutex<Option<mpsc::SyncSender<Vec<u8>>>>;

#[derive(Clone, Default)]
pub(super) struct AudioRoutes {
    slots: Arc<Mutex<HashMap<String, Weak<AudioSlot>>>>,
}

impl AudioRoutes {
    pub fn attach(&self, key: &str, sender: mpsc::SyncSender<Vec<u8>>) -> bool {
        let slot = self.slots.lock().unwrap().get(key).and_then(Weak::upgrade);
        let Some(slot) = slot else { return false };
        let mut current = slot.lock().unwrap();
        // One audio socket per session. A duplicate must not replace a live peer.
        if current.is_some() {
            return false;
        }
        *current = Some(sender);
        true
    }
}

pub(super) struct SessionAudio {
    routes: AudioRoutes,
    key: Option<String>,
    slot: Arc<AudioSlot>,
}

impl SessionAudio {
    pub fn new(routes: AudioRoutes) -> Self {
        Self {
            routes,
            key: None,
            slot: Arc::new(Mutex::new(None)),
        }
    }

    pub fn register(&mut self, token: &[u8]) {
        // Hello tokens are 256-bit random capabilities. Older fixtures/peers may
        // use an empty token; retain main-channel audio for those connections.
        if token.len() != 32 {
            return;
        }
        let key: String = token.iter().map(|b| format!("{b:02x}")).collect();
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.unregister();
        self.routes
            .slots
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::downgrade(&self.slot));
        self.key = Some(key);
    }

    /// Preserve the payload for main-channel fallback when no audio socket is
    /// attached. A full dedicated queue remains a bounded, explicit drop.
    pub fn send(&self, payload: Vec<u8>) -> Result<(), mpsc::TrySendError<Vec<u8>>> {
        let mut slot = self.slot.lock().unwrap();
        let result = match slot.as_ref() {
            Some(sender) => sender.try_send(payload),
            None => return Err(mpsc::TrySendError::Disconnected(payload)),
        };
        if matches!(result, Err(mpsc::TrySendError::Disconnected(_))) {
            *slot = None;
        }
        result
    }

    fn unregister(&mut self) {
        if let Some(key) = self.key.take() {
            let mut slots = self.routes.slots.lock().unwrap();
            if slots
                .get(&key)
                .is_some_and(|v| Weak::ptr_eq(v, &Arc::downgrade(&self.slot)))
            {
                slots.remove(&key);
            }
        }
        *self.slot.lock().unwrap() = None;
    }
}

impl Drop for SessionAudio {
    fn drop(&mut self) {
        self.unregister();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_is_bound_to_its_live_session_and_closes_with_it() {
        let routes = AudioRoutes::default();
        let mut first = SessionAudio::new(routes.clone());
        let mut second = SessionAudio::new(routes.clone());
        first.register(&[1; 32]);
        second.register(&[2; 32]);
        let (tx1, rx1) = mpsc::sync_channel(2);
        let (tx2, rx2) = mpsc::sync_channel(2);
        assert!(routes.attach(&"01".repeat(32), tx1));
        assert!(routes.attach(&"02".repeat(32), tx2));
        first.send(vec![1]).unwrap();
        second.send(vec![2]).unwrap();
        assert_eq!(rx1.recv().unwrap(), [1]);
        assert_eq!(rx2.recv().unwrap(), [2]);
        drop(first);
        assert!(matches!(
            rx1.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        let (stale, _) = mpsc::sync_channel(1);
        assert!(!routes.attach(&"01".repeat(32), stale));
        second.send(vec![3]).unwrap();
        assert_eq!(rx2.recv().unwrap(), [3]);
    }

    #[test]
    fn duplicate_unknown_and_empty_tokens_cannot_take_over_audio() {
        let routes = AudioRoutes::default();
        let mut audio = SessionAudio::new(routes.clone());
        audio.register(&[]);
        let (invalid, _) = mpsc::sync_channel(1);
        assert!(!routes.attach("", invalid));
        audio.register(&[3; 32]);
        let (tx, rx) = mpsc::sync_channel(1);
        assert!(routes.attach(&"03".repeat(32), tx));
        let (duplicate, duplicate_rx) = mpsc::sync_channel(1);
        assert!(!routes.attach(&"03".repeat(32), duplicate));
        assert!(matches!(
            duplicate_rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        audio.send(vec![7]).unwrap();
        assert_eq!(rx.recv().unwrap(), [7]);
    }

    #[test]
    fn full_queue_stays_bounded_and_closed_audio_returns_payload_for_fallback() {
        let routes = AudioRoutes::default();
        let mut audio = SessionAudio::new(routes.clone());
        audio.register(&[4; 32]);
        assert!(
            matches!(audio.send(vec![1]), Err(mpsc::TrySendError::Disconnected(v)) if v == [1])
        );
        let (tx, rx) = mpsc::sync_channel(1);
        assert!(routes.attach(&"04".repeat(32), tx));
        audio.send(vec![2]).unwrap();
        assert!(matches!(audio.send(vec![3]), Err(mpsc::TrySendError::Full(v)) if v == [3]));
        assert_eq!(rx.recv().unwrap(), [2]);
        drop(rx);
        assert!(
            matches!(audio.send(vec![4]), Err(mpsc::TrySendError::Disconnected(v)) if v == [4])
        );
        let (replacement, replacement_rx) = mpsc::sync_channel(1);
        assert!(routes.attach(&"04".repeat(32), replacement));
        audio.send(vec![5]).unwrap();
        assert_eq!(replacement_rx.recv().unwrap(), [5]);
    }
}
