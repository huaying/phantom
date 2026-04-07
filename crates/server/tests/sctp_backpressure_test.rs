/// Tests for the SCTP backpressure and chunking logic.

#[test]
fn chunk_framing_small_message() {
    // Messages ≤ 16KB should not be chunked
    let data = vec![42u8; 1000];
    let chunks = chunk_message(&data, 16_384);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], data); // no framing header for small messages
}

#[test]
fn chunk_framing_large_message() {
    // 70KB message should be split into chunks
    let data = vec![0xAB_u8; 70_000];
    let chunk_size = 16_384;
    let chunks = chunk_message(&data, chunk_size);

    // Each chunk has 4-byte header + payload
    assert!(chunks.len() >= 5, "70KB / ~16KB = at least 5 chunks, got {}", chunks.len());

    // Verify framing: each chunk starts with [total_len as u32 LE]
    for chunk in &chunks {
        assert!(chunk.len() <= chunk_size);
        let total = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as usize;
        assert_eq!(total, 70_000, "total_len header should be original message size");
    }

    // Reassemble and verify
    let mut reassembled = Vec::new();
    for chunk in &chunks {
        reassembled.extend_from_slice(&chunk[4..]); // skip 4-byte header
    }
    assert_eq!(reassembled.len(), 70_000);
    assert!(reassembled.iter().all(|&b| b == 0xAB));
}

#[test]
fn chunk_framing_exact_boundary() {
    // Message exactly at chunk size should not be chunked
    let data = vec![0xFF_u8; 16_384];
    let chunks = chunk_message(&data, 16_384);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], data);
}

#[test]
fn chunk_framing_one_byte_over() {
    // Message 1 byte over chunk size should be split into 2 chunks
    let data = vec![0xCC_u8; 16_385];
    let chunks = chunk_message(&data, 16_384);
    assert_eq!(chunks.len(), 2, "16385 bytes should split into 2 chunks");

    // Reassemble
    let mut reassembled = Vec::new();
    for chunk in &chunks {
        let total = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as usize;
        assert_eq!(total, 16_385);
        reassembled.extend_from_slice(&chunk[4..]);
    }
    assert_eq!(reassembled.len(), 16_385);
}

/// Simulate the chunking logic from transport_webrtc.rs
fn chunk_message(data: &[u8], chunk_size: usize) -> Vec<Vec<u8>> {
    if data.len() <= chunk_size {
        return vec![data.to_vec()];
    }

    let total = data.len() as u32;
    let mut chunks = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let end = (offset + chunk_size - 4).min(data.len());
        let mut chunk = Vec::with_capacity(4 + (end - offset));
        chunk.extend_from_slice(&total.to_le_bytes());
        chunk.extend_from_slice(&data[offset..end]);
        chunks.push(chunk);
        offset = end;
    }
    chunks
}
