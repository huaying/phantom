//! Server-side file transfer handling.
//!
//! Receives files from clients into ~/Downloads/phantom/.
//! Can also send files to the connected client.

use anyhow::Result;
use phantom_core::file_transfer::{FileTransferManager, IncrementalHasher, CHUNK_SIZE};
use phantom_core::protocol::Message;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};

/// Download directory for files received from clients.
/// Works across Windows/Linux/macOS and in service mode (Session 0).
fn download_dir() -> PathBuf {
    // Try platform-specific user directories first
    if let Some(dir) = dirs_impl::download_dir() {
        return dir.join("phantom");
    }
    // Fallback
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join("Downloads").join("phantom")
}

/// Platform-specific download directory detection.
mod dirs_impl {
    use std::path::PathBuf;

    pub fn download_dir() -> Option<PathBuf> {
        #[cfg(target_os = "windows")]
        {
            // In service mode (Session 0), USERPROFILE points to SYSTEM profile.
            // Query the active console session's user profile instead.
            if let Some(dir) = windows_user_downloads() {
                return Some(dir);
            }
        }
        // Linux/macOS: $HOME/Downloads
        std::env::var("HOME").ok().map(|h| PathBuf::from(h).join("Downloads"))
    }

    #[cfg(target_os = "windows")]
    fn windows_user_downloads() -> Option<PathBuf> {
        // Try USERPROFILE first (works in console mode and user sessions)
        if let Ok(profile) = std::env::var("USERPROFILE") {
            let path = PathBuf::from(&profile);
            if !profile.contains("systemprofile") {
                return Some(path.join("Downloads"));
            }
        }
        // Service mode (Session 0): find the active console user's Downloads.
        // Query username from the active console session via WTS API.
        if let Some(dir) = service_mode_user_downloads() {
            return Some(dir);
        }
        // Fallback
        let public = PathBuf::from(r"C:\Users\Public\Downloads");
        if public.exists() {
            return Some(public);
        }
        None
    }

    #[cfg(target_os = "windows")]
    fn service_mode_user_downloads() -> Option<PathBuf> {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        unsafe {
            extern "system" {
                fn WTSGetActiveConsoleSessionId() -> u32;
                fn WTSQuerySessionInformationW(
                    hServer: *mut std::ffi::c_void,
                    SessionId: u32,
                    WTSInfoClass: i32,
                    ppBuffer: *mut *mut u16,
                    pBytesReturned: *mut u32,
                ) -> i32;
                fn WTSFreeMemory(pMemory: *mut std::ffi::c_void);
            }
            const WTSUserName: i32 = 5;

            let session_id = WTSGetActiveConsoleSessionId();
            if session_id == 0xFFFFFFFF {
                return None;
            }

            let mut buf: *mut u16 = std::ptr::null_mut();
            let mut len: u32 = 0;
            if WTSQuerySessionInformationW(
                std::ptr::null_mut(),
                session_id,
                WTSUserName,
                &mut buf,
                &mut len,
            ) == 0
            {
                return None;
            }

            let username = {
                let slice = std::slice::from_raw_parts(buf, (len / 2) as usize);
                let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                OsString::from_wide(&slice[..end])
                    .to_string_lossy()
                    .to_string()
            };
            WTSFreeMemory(buf as *mut _);

            if username.is_empty() {
                return None;
            }

            let path = PathBuf::from(format!(r"C:\Users\{}\Downloads", username));
            if path.exists() {
                Some(path)
            } else {
                None
            }
        }
    }
}

/// Messages from the file-send background thread back to the session.
pub enum FileSendEvent {
    /// Send this message to the remote.
    Send(Message),
    /// Sending finished for this transfer.
    Done(u64),
    /// Error during send.
    Error(u64, String),
}

/// Handles file transfers on the server side.
pub struct ServerFileTransfer {
    manager: FileTransferManager,
    /// Receivers currently writing to disk. Key = transfer_id.
    receivers: HashMap<u64, FileReceiver>,
    /// Channel for messages from background file-send threads.
    send_event_rx: mpsc::Receiver<FileSendEvent>,
    send_event_tx: mpsc::Sender<FileSendEvent>,
    /// Signals for send threads waiting for FileAccept.
    accept_signals: HashMap<u64, Arc<(Mutex<bool>, Condvar)>>,
}

struct FileReceiver {
    file: fs::File,
    temp_path: PathBuf,
    final_path: PathBuf,
    hasher: IncrementalHasher,
    received: u64,
    expected: u64,
}

impl ServerFileTransfer {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            manager: FileTransferManager::new(),
            receivers: HashMap::new(),
            send_event_rx: rx,
            send_event_tx: tx,
            accept_signals: HashMap::new(),
        }
    }

    /// Handle a FileOffer from the client (they want to send us a file).
    /// Returns a FileAccept message to send back.
    pub fn on_file_offer(&mut self, transfer_id: u64, name: &str, size: u64) -> Result<Message> {
        // Sanitize filename
        let safe_name = Path::new(name)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("file_{transfer_id}"));

        let dir = download_dir();
        fs::create_dir_all(&dir)?;

        let final_path = dir.join(&safe_name);
        let temp_path = dir.join(format!(".{safe_name}.phantom_tmp"));

        let file = fs::File::create(&temp_path)?;

        self.manager.on_offer_received(transfer_id, safe_name, size);
        self.manager.on_accept(transfer_id);

        self.receivers.insert(
            transfer_id,
            FileReceiver {
                file,
                temp_path,
                final_path,
                hasher: IncrementalHasher::new(),
                received: 0,
                expected: size,
            },
        );

        tracing::info!(transfer_id, name, size, "file offer accepted");
        Ok(Message::FileAccept { transfer_id })
    }

    /// Handle a FileChunk from the client.
    pub fn on_file_chunk(&mut self, transfer_id: u64, offset: u64, data: &[u8]) -> Result<()> {
        if let Some(recv) = self.receivers.get_mut(&transfer_id) {
            if offset != recv.received {
                anyhow::bail!(
                    "file chunk out of order: expected offset {}, got {offset}",
                    recv.received
                );
            }
            recv.file.write_all(data)?;
            recv.hasher.update(data);
            recv.received += data.len() as u64;
            self.manager.on_chunk(transfer_id, data.len() as u64);
        }
        Ok(())
    }

    /// Handle a FileDone from the client. Verify hash and move to final path.
    pub fn on_file_done(&mut self, transfer_id: u64, sha256: &[u8; 32]) -> Result<()> {
        if let Some(recv) = self.receivers.remove(&transfer_id) {
            drop(recv.file); // flush and close
            let computed = recv.hasher.finalize();
            if &computed != sha256 {
                // Hash mismatch — delete temp file
                let _ = fs::remove_file(&recv.temp_path);
                tracing::error!(
                    transfer_id,
                    "file hash mismatch: expected {:x?}, got {:x?}",
                    sha256,
                    computed
                );
                anyhow::bail!("SHA-256 mismatch for transfer {transfer_id}");
            }

            // Handle name collision: add (1), (2), etc.
            let final_path = unique_path(&recv.final_path);
            fs::rename(&recv.temp_path, &final_path)?;

            let info = self.manager.on_done(transfer_id);
            tracing::info!(
                transfer_id,
                path = %final_path.display(),
                size = recv.expected,
                name = info.as_ref().map(|i| i.name.as_str()).unwrap_or("?"),
                "file received successfully"
            );
            self.manager.cleanup();
        }
        Ok(())
    }

    /// Handle a FileCancel from the client.
    pub fn on_file_cancel(&mut self, transfer_id: u64, reason: &str) {
        if let Some(recv) = self.receivers.remove(&transfer_id) {
            drop(recv.file);
            let _ = fs::remove_file(&recv.temp_path);
        }
        self.manager.on_cancel(transfer_id);
        tracing::warn!(transfer_id, reason, "file transfer cancelled");
    }

    /// Handle a FileAccept from the client (they accepted our offer to send).
    pub fn on_file_accept(&mut self, transfer_id: u64) {
        self.manager.on_accept(transfer_id);
        // Signal the send thread to start
        if let Some(signal) = self.accept_signals.remove(&transfer_id) {
            let (lock, cvar) = &*signal;
            if let Ok(mut accepted) = lock.lock() {
                *accepted = true;
                cvar.notify_one();
            }
        }
    }

    /// Start sending a file to the connected client.
    /// Spawns a background thread that reads the file and sends chunks.
    /// The FileOffer and all chunks will come through drain_send_events().
    pub fn initiate_send(&mut self, path: &Path) -> Result<u64> {
        let metadata = fs::metadata(path)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        let size = metadata.len();

        let transfer_id = self.manager.offer_send(name.clone(), size);
        let file_path = path.to_path_buf();
        let event_tx = self.send_event_tx.clone();

        // Create accept signal
        let signal = Arc::new((Mutex::new(false), Condvar::new()));
        self.accept_signals.insert(transfer_id, Arc::clone(&signal));

        // Send the offer through our event channel
        let _ = event_tx.send(FileSendEvent::Send(Message::FileOffer {
            transfer_id,
            name: name.clone(),
            size,
        }));

        // Spawn background thread — waits for accept signal before sending
        std::thread::Builder::new()
            .name(format!("file-send-{transfer_id}"))
            .spawn(move || {
                // Wait for FileAccept (up to 30s)
                let (lock, cvar) = &*signal;
                let accepted = match lock.lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        let _ = event_tx.send(FileSendEvent::Error(
                            transfer_id,
                            "accept signal mutex poisoned".to_string(),
                        ));
                        return;
                    }
                };
                let result = match cvar.wait_timeout_while(
                    accepted,
                    std::time::Duration::from_secs(30),
                    |a| !*a,
                ) {
                    Ok(r) => r,
                    Err(_) => {
                        let _ = event_tx.send(FileSendEvent::Error(
                            transfer_id,
                            "accept signal mutex poisoned during wait".to_string(),
                        ));
                        return;
                    }
                };
                if !*result.0 {
                    let _ = event_tx.send(FileSendEvent::Error(
                        transfer_id,
                        "file offer not accepted within 30s".to_string(),
                    ));
                    return;
                }

                if let Err(e) = send_file_chunks(transfer_id, &file_path, &event_tx) {
                    let _ = event_tx.send(FileSendEvent::Error(transfer_id, format!("{e}")));
                }
            })?;

        tracing::info!(transfer_id, %name, size, "file send initiated");
        Ok(transfer_id)
    }

    /// Drain pending messages from background send threads.
    /// Returns messages that should be sent to the remote.
    pub fn drain_send_events(&mut self) -> Vec<Message> {
        let mut msgs = Vec::new();
        while let Ok(event) = self.send_event_rx.try_recv() {
            match event {
                FileSendEvent::Send(msg) => {
                    if let Message::FileChunk {
                        transfer_id,
                        ref data,
                        ..
                    } = msg
                    {
                        self.manager.on_chunk(transfer_id, data.len() as u64);
                    }
                    msgs.push(msg);
                }
                FileSendEvent::Done(id) => {
                    self.manager.on_done(id);
                    self.manager.cleanup();
                    tracing::info!(transfer_id = id, "file send complete");
                }
                FileSendEvent::Error(id, reason) => {
                    self.manager.on_cancel(id);
                    tracing::error!(transfer_id = id, reason, "file send failed");
                    msgs.push(Message::FileCancel {
                        transfer_id: id,
                        reason,
                    });
                }
            }
        }
        msgs
    }
}

impl Default for ServerFileTransfer {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a file and send it as chunks via the event channel.
fn send_file_chunks(transfer_id: u64, path: &Path, tx: &mpsc::Sender<FileSendEvent>) -> Result<()> {
    let mut file = fs::File::open(path)?;
    let mut hasher = IncrementalHasher::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut offset = 0u64;

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);

        tx.send(FileSendEvent::Send(Message::FileChunk {
            transfer_id,
            offset,
            data: buf[..n].to_vec(),
        }))?;
        offset += n as u64;
    }

    let sha256 = hasher.finalize();
    tx.send(FileSendEvent::Send(Message::FileDone {
        transfer_id,
        sha256,
    }))?;
    tx.send(FileSendEvent::Done(transfer_id))?;
    Ok(())
}

/// Generate a unique file path by appending (1), (2), etc. if the file exists.
fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }

    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = path.parent().unwrap_or(Path::new("."));

    for i in 1u32.. {
        let candidate = parent.join(format!("{stem} ({i}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Fallback (should never reach here)
    path.to_path_buf()
}
