//! Shared file transfer state machine and utilities.
//!
//! Both server and client use `FileTransferManager` to track active transfers.
//! The actual I/O (reading files, writing chunks) happens in the server/client
//! crate-specific modules.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Chunk size for file transfer: 256 KB.
pub const CHUNK_SIZE: usize = 256 * 1024;

static NEXT_TRANSFER_ID: AtomicU64 = AtomicU64::new(1);

/// Generate a unique transfer ID.
pub fn next_transfer_id() -> u64 {
    NEXT_TRANSFER_ID.fetch_add(1, Ordering::Relaxed)
}

/// State of a single file transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    /// We offered to send (or received an offer). Waiting for accept/reject.
    Offered,
    /// Accepted — data transfer in progress.
    Accepted,
    /// All chunks sent/received, waiting for FileDone.
    InProgress,
    /// Transfer completed successfully.
    Done,
    /// Transfer cancelled.
    Cancelled,
}

/// Direction of transfer from our perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    /// We are sending the file.
    Sending,
    /// We are receiving the file.
    Receiving,
}

/// Metadata for a tracked transfer.
#[derive(Debug, Clone)]
pub struct TransferInfo {
    pub transfer_id: u64,
    pub name: String,
    pub size: u64,
    pub state: TransferState,
    pub direction: TransferDirection,
    pub bytes_transferred: u64,
}

/// Tracks all active file transfers.
pub struct FileTransferManager {
    transfers: HashMap<u64, TransferInfo>,
}

impl FileTransferManager {
    pub fn new() -> Self {
        Self {
            transfers: HashMap::new(),
        }
    }

    /// Register an outbound offer (we want to send a file).
    pub fn offer_send(&mut self, name: String, size: u64) -> u64 {
        let id = next_transfer_id();
        self.transfers.insert(
            id,
            TransferInfo {
                transfer_id: id,
                name,
                size,
                state: TransferState::Offered,
                direction: TransferDirection::Sending,
                bytes_transferred: 0,
            },
        );
        id
    }

    /// Register an inbound offer (remote wants to send us a file).
    pub fn on_offer_received(&mut self, transfer_id: u64, name: String, size: u64) {
        self.transfers.insert(
            transfer_id,
            TransferInfo {
                transfer_id,
                name,
                size,
                state: TransferState::Offered,
                direction: TransferDirection::Receiving,
                bytes_transferred: 0,
            },
        );
    }

    /// Mark a transfer as accepted.
    pub fn on_accept(&mut self, transfer_id: u64) -> bool {
        if let Some(t) = self.transfers.get_mut(&transfer_id) {
            if t.state == TransferState::Offered {
                t.state = TransferState::Accepted;
                return true;
            }
        }
        false
    }

    /// Record received bytes for a transfer.
    pub fn on_chunk(&mut self, transfer_id: u64, chunk_len: u64) -> bool {
        if let Some(t) = self.transfers.get_mut(&transfer_id) {
            if t.state == TransferState::Accepted || t.state == TransferState::InProgress {
                t.state = TransferState::InProgress;
                t.bytes_transferred += chunk_len;
                return true;
            }
        }
        false
    }

    /// Mark a transfer as done.
    pub fn on_done(&mut self, transfer_id: u64) -> Option<TransferInfo> {
        if let Some(t) = self.transfers.get_mut(&transfer_id) {
            t.state = TransferState::Done;
            return Some(t.clone());
        }
        None
    }

    /// Cancel a transfer.
    pub fn on_cancel(&mut self, transfer_id: u64) -> bool {
        if let Some(t) = self.transfers.get_mut(&transfer_id) {
            t.state = TransferState::Cancelled;
            return true;
        }
        false
    }

    /// Look up a transfer.
    pub fn get(&self, transfer_id: u64) -> Option<&TransferInfo> {
        self.transfers.get(&transfer_id)
    }

    /// Remove completed/cancelled transfers.
    pub fn cleanup(&mut self) {
        self.transfers
            .retain(|_, t| t.state != TransferState::Done && t.state != TransferState::Cancelled);
    }
}

impl Default for FileTransferManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute SHA-256 of a file by reading it in chunks.
pub fn sha256_file(path: &std::path::Path) -> std::io::Result<[u8; 32]> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Incrementally compute SHA-256 as chunks arrive.
pub struct IncrementalHasher {
    hasher: Sha256,
}

impl IncrementalHasher {
    pub fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.hasher.update(data);
    }

    pub fn finalize(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }
}

impl Default for IncrementalHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_state_machine_send() {
        let mut mgr = FileTransferManager::new();
        let id = mgr.offer_send("test.bin".into(), 1024);
        assert_eq!(mgr.get(id).unwrap().state, TransferState::Offered);

        assert!(mgr.on_accept(id));
        assert_eq!(mgr.get(id).unwrap().state, TransferState::Accepted);

        assert!(mgr.on_chunk(id, 512));
        assert_eq!(mgr.get(id).unwrap().state, TransferState::InProgress);
        assert_eq!(mgr.get(id).unwrap().bytes_transferred, 512);

        assert!(mgr.on_chunk(id, 512));
        assert_eq!(mgr.get(id).unwrap().bytes_transferred, 1024);

        let info = mgr.on_done(id).unwrap();
        assert_eq!(info.state, TransferState::Done);
        assert_eq!(info.bytes_transferred, 1024);
    }

    #[test]
    fn transfer_state_machine_receive() {
        let mut mgr = FileTransferManager::new();
        mgr.on_offer_received(99, "image.png".into(), 2048);
        assert_eq!(mgr.get(99).unwrap().state, TransferState::Offered);
        assert_eq!(mgr.get(99).unwrap().direction, TransferDirection::Receiving);

        assert!(mgr.on_accept(99));
        assert!(mgr.on_chunk(99, 1024));
        assert!(mgr.on_chunk(99, 1024));
        let info = mgr.on_done(99).unwrap();
        assert_eq!(info.bytes_transferred, 2048);
    }

    #[test]
    fn transfer_cancel() {
        let mut mgr = FileTransferManager::new();
        let id = mgr.offer_send("cancel.txt".into(), 100);
        assert!(mgr.on_cancel(id));
        assert_eq!(mgr.get(id).unwrap().state, TransferState::Cancelled);
    }

    #[test]
    fn transfer_cleanup() {
        let mut mgr = FileTransferManager::new();
        let id1 = mgr.offer_send("done.txt".into(), 100);
        let id2 = mgr.offer_send("active.txt".into(), 200);
        mgr.on_accept(id1);
        mgr.on_done(id1);
        mgr.cleanup();
        assert!(mgr.get(id1).is_none());
        assert!(mgr.get(id2).is_some());
    }

    #[test]
    fn incremental_hasher() {
        let mut h = IncrementalHasher::new();
        h.update(b"hello ");
        h.update(b"world");
        let hash = h.finalize();

        let mut full = Sha256::new();
        full.update(b"hello world");
        let expected: [u8; 32] = full.finalize().into();
        assert_eq!(hash, expected);
    }

    #[test]
    fn reject_chunk_before_accept() {
        let mut mgr = FileTransferManager::new();
        let id = mgr.offer_send("test.bin".into(), 100);
        // Chunk before accept should fail
        assert!(!mgr.on_chunk(id, 50));
    }

    #[test]
    fn double_accept_rejected() {
        let mut mgr = FileTransferManager::new();
        let id = mgr.offer_send("test.bin".into(), 100);
        assert!(mgr.on_accept(id));
        // Second accept on already-accepted transfer should fail
        assert!(!mgr.on_accept(id));
    }
}
