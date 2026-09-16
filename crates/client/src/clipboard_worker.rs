use std::sync::mpsc;
use std::time::{Duration, Instant};

const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(250);

enum ClipboardCommand {
    SetText(String),
    ReadForPaste,
}

pub enum ClipboardEvent {
    Observed(String),
    Paste(String),
}

pub struct ClipboardWorker {
    command_tx: mpsc::Sender<ClipboardCommand>,
    event_rx: mpsc::Receiver<ClipboardEvent>,
}

impl ClipboardWorker {
    pub fn spawn() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();

        std::thread::Builder::new()
            .name("client-clipboard".into())
            .spawn(move || run_clipboard_worker(command_rx, event_tx))
            .expect("spawn clipboard worker");

        Self {
            command_tx,
            event_rx,
        }
    }

    pub fn set_text(&self, text: String) {
        let _ = self.command_tx.send(ClipboardCommand::SetText(text));
    }

    pub fn request_paste(&self) -> bool {
        self.command_tx.send(ClipboardCommand::ReadForPaste).is_ok()
    }

    pub fn drain_events(&self) -> impl Iterator<Item = ClipboardEvent> + '_ {
        self.event_rx.try_iter()
    }
}

fn run_clipboard_worker(
    command_rx: mpsc::Receiver<ClipboardCommand>,
    event_tx: mpsc::Sender<ClipboardEvent>,
) {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        tracing::warn!("system clipboard unavailable");
        return;
    };

    let mut last_observed = None;
    let mut next_poll = Instant::now();

    loop {
        let timeout = next_poll.saturating_duration_since(Instant::now());
        match command_rx.recv_timeout(timeout) {
            Ok(ClipboardCommand::SetText(text)) => {
                if clipboard.set_text(&text).is_ok() {
                    last_observed = Some(text);
                }
            }
            Ok(ClipboardCommand::ReadForPaste) => {
                if let Ok(text) = clipboard.get_text() {
                    if event_tx.send(ClipboardEvent::Paste(text)).is_err() {
                        break;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(text) = clipboard.get_text() {
                    if last_observed.as_ref() != Some(&text) {
                        last_observed = Some(text.clone());
                        if event_tx.send(ClipboardEvent::Observed(text)).is_err() {
                            break;
                        }
                    }
                }
                next_poll = Instant::now() + CLIPBOARD_POLL_INTERVAL;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}
