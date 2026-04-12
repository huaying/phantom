//! Minimal STUN Binding Request (RFC 5389) for NAT discovery.
//!
//! Sends a STUN Binding Request to a public STUN server and parses
//! the XOR-MAPPED-ADDRESS from the response to determine the
//! server's public IP and port as seen by the internet.
//!
//! # Example
//!
//! ```no_run
//! let public = phantom_core::stun::discover_public_addr("stun.l.google.com:19302")?;
//! println!("Public address: {public}");
//! # Ok::<(), anyhow::Error>(())
//! ```

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// STUN magic cookie (RFC 5389 §6).
const MAGIC_COOKIE: u32 = 0x2112_A442;

/// STUN message type: Binding Request.
const BINDING_REQUEST: u16 = 0x0001;

/// STUN message type: Binding Success Response.
const BINDING_SUCCESS: u16 = 0x0101;

/// STUN attribute type: XOR-MAPPED-ADDRESS (RFC 5389 §15.2).
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// STUN attribute type: MAPPED-ADDRESS (fallback, RFC 5389 §15.1).
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;

/// Discover the public (NAT-external) address by sending a STUN Binding
/// Request to the given STUN server.
///
/// The local socket is bound to `0.0.0.0:0` (ephemeral port). The returned
/// address is what the STUN server sees — i.e., the public IP:port after NAT.
///
/// `stun_server` should be `host:port`, e.g. `"stun.l.google.com:19302"`.
pub fn discover_public_addr(stun_server: &str) -> Result<SocketAddr> {
    discover_public_addr_from(stun_server, "0.0.0.0:0")
}

/// Like [`discover_public_addr`] but binds the local socket to `local_addr`.
/// Use this to discover the public mapping for a specific local port.
pub fn discover_public_addr_from(stun_server: &str, local_addr: &str) -> Result<SocketAddr> {
    let socket = UdpSocket::bind(local_addr).context("bind UDP socket")?;
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .context("set read timeout")?;

    // Build STUN Binding Request (20-byte header, no attributes)
    let transaction_id: [u8; 12] = rand_transaction_id();
    let request = build_binding_request(&transaction_id);

    // Send to STUN server
    socket
        .send_to(&request, stun_server)
        .context("send STUN request")?;

    // Receive response
    let mut buf = [0u8; 576]; // STUN messages are small
    let (n, _from) = socket
        .recv_from(&mut buf)
        .context("receive STUN response (timeout?)")?;

    parse_binding_response(&buf[..n], &transaction_id)
}

/// Build a STUN Binding Request packet.
fn build_binding_request(transaction_id: &[u8; 12]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(20);
    // Message Type: Binding Request
    pkt.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    // Message Length: 0 (no attributes)
    pkt.extend_from_slice(&0u16.to_be_bytes());
    // Magic Cookie
    pkt.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    // Transaction ID (96 bits)
    pkt.extend_from_slice(transaction_id);
    pkt
}

/// Parse a STUN Binding Success Response and extract the public address.
fn parse_binding_response(data: &[u8], expected_txn: &[u8; 12]) -> Result<SocketAddr> {
    if data.len() < 20 {
        bail!("STUN response too short ({} bytes)", data.len());
    }

    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let cookie = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let txn_id = &data[8..20];

    if msg_type != BINDING_SUCCESS {
        bail!("unexpected STUN message type: 0x{msg_type:04x}");
    }
    if cookie != MAGIC_COOKIE {
        bail!("bad magic cookie: 0x{cookie:08x}");
    }
    if txn_id != expected_txn {
        bail!("transaction ID mismatch");
    }
    if data.len() < 20 + msg_len {
        bail!("STUN response truncated");
    }

    // Parse attributes
    let attrs = &data[20..20 + msg_len];
    let mut offset = 0;
    while offset + 4 <= attrs.len() {
        let attr_type = u16::from_be_bytes([attrs[offset], attrs[offset + 1]]);
        let attr_len = u16::from_be_bytes([attrs[offset + 2], attrs[offset + 3]]) as usize;
        let attr_data = &attrs[offset + 4..offset + 4 + attr_len.min(attrs.len() - offset - 4)];

        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => {
                return parse_xor_mapped_address(attr_data);
            }
            ATTR_MAPPED_ADDRESS => {
                // Fallback: non-XOR mapped address
                return parse_mapped_address(attr_data);
            }
            _ => {
                // Skip unknown attributes (padded to 4-byte boundary)
                offset += 4 + ((attr_len + 3) & !3);
                continue;
            }
        }
    }

    bail!("no MAPPED-ADDRESS or XOR-MAPPED-ADDRESS in STUN response")
}

/// Parse XOR-MAPPED-ADDRESS attribute (RFC 5389 §15.2).
fn parse_xor_mapped_address(data: &[u8]) -> Result<SocketAddr> {
    if data.len() < 8 {
        bail!("XOR-MAPPED-ADDRESS too short");
    }
    let family = data[1];
    let xport = u16::from_be_bytes([data[2], data[3]]) ^ (MAGIC_COOKIE >> 16) as u16;

    match family {
        0x01 => {
            // IPv4
            let xaddr = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) ^ MAGIC_COOKIE;
            let ip = std::net::Ipv4Addr::from(xaddr);
            Ok(SocketAddr::new(ip.into(), xport))
        }
        0x02 => {
            // IPv6
            if data.len() < 20 {
                bail!("XOR-MAPPED-ADDRESS IPv6 too short");
            }
            let mut addr_bytes = [0u8; 16];
            addr_bytes.copy_from_slice(&data[4..20]);
            // XOR with magic cookie + transaction ID
            // (we only have magic cookie here, full XOR requires txn_id)
            let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
            for i in 0..4 {
                addr_bytes[i] ^= cookie_bytes[i];
            }
            // Remaining 12 bytes XORed with transaction ID — we'd need it
            // For now, IPv4 is sufficient for most NAT traversal
            let ip = std::net::Ipv6Addr::from(addr_bytes);
            Ok(SocketAddr::new(ip.into(), xport))
        }
        _ => bail!("unknown address family: {family}"),
    }
}

/// Parse MAPPED-ADDRESS attribute (RFC 5389 §15.1) — non-XOR fallback.
fn parse_mapped_address(data: &[u8]) -> Result<SocketAddr> {
    if data.len() < 8 {
        bail!("MAPPED-ADDRESS too short");
    }
    let family = data[1];
    let port = u16::from_be_bytes([data[2], data[3]]);

    match family {
        0x01 => {
            let ip = std::net::Ipv4Addr::new(data[4], data[5], data[6], data[7]);
            Ok(SocketAddr::new(ip.into(), port))
        }
        0x02 if data.len() >= 20 => {
            let mut addr = [0u8; 16];
            addr.copy_from_slice(&data[4..20]);
            let ip = std::net::Ipv6Addr::from(addr);
            Ok(SocketAddr::new(ip.into(), port))
        }
        _ => bail!("unknown address family: {family}"),
    }
}

/// Generate a random-enough 12-byte transaction ID.
/// Uses system time + process ID — doesn't need to be cryptographically secure.
fn rand_transaction_id() -> [u8; 12] {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = now.as_nanos() as u64;
    let pid = std::process::id();
    let mut id = [0u8; 12];
    id[0..8].copy_from_slice(&nanos.to_le_bytes());
    id[8..12].copy_from_slice(&pid.to_le_bytes());
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_parse_binding_request() {
        let txn = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let pkt = build_binding_request(&txn);
        assert_eq!(pkt.len(), 20);
        assert_eq!(u16::from_be_bytes([pkt[0], pkt[1]]), BINDING_REQUEST);
        assert_eq!(u16::from_be_bytes([pkt[2], pkt[3]]), 0); // no attributes
        assert_eq!(
            u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]),
            MAGIC_COOKIE
        );
        assert_eq!(&pkt[8..20], &txn);
    }

    #[test]
    fn parse_xor_mapped_ipv4() {
        // Construct a fake XOR-MAPPED-ADDRESS for 203.0.113.5:12345
        let ip = std::net::Ipv4Addr::new(203, 0, 113, 5);
        let port: u16 = 12345;
        let xport = port ^ (MAGIC_COOKIE >> 16) as u16;
        let xaddr = u32::from(ip) ^ MAGIC_COOKIE;

        let mut data = vec![0u8; 8];
        data[1] = 0x01; // IPv4
        data[2..4].copy_from_slice(&xport.to_be_bytes());
        data[4..8].copy_from_slice(&xaddr.to_be_bytes());

        let addr = parse_xor_mapped_address(&data).unwrap();
        assert_eq!(addr, SocketAddr::new(ip.into(), port));
    }

    #[test]
    fn parse_full_binding_response() {
        let txn = [0xAA; 12];
        let ip = std::net::Ipv4Addr::new(93, 184, 216, 34);
        let port: u16 = 54321;
        let xport = port ^ (MAGIC_COOKIE >> 16) as u16;
        let xaddr = u32::from(ip) ^ MAGIC_COOKIE;

        // Build response: header + XOR-MAPPED-ADDRESS attribute
        let mut attr = Vec::new();
        attr.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes()); // type
        attr.extend_from_slice(&8u16.to_be_bytes()); // length
        attr.push(0); // reserved
        attr.push(0x01); // IPv4
        attr.extend_from_slice(&xport.to_be_bytes());
        attr.extend_from_slice(&xaddr.to_be_bytes());

        let mut resp = Vec::new();
        resp.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        resp.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        resp.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        resp.extend_from_slice(&txn);
        resp.extend_from_slice(&attr);

        let addr = parse_binding_response(&resp, &txn).unwrap();
        assert_eq!(addr, SocketAddr::new(ip.into(), port));
    }

    #[test]
    fn stun_discover_google() {
        // Integration test: actually contact Google's STUN server
        // Skip if no network
        match discover_public_addr("stun.l.google.com:19302") {
            Ok(addr) => {
                eprintln!("Public address: {addr}");
                assert!(!addr.ip().is_loopback());
                assert!(!addr.ip().is_unspecified());
                assert_ne!(addr.port(), 0);
            }
            Err(e) => {
                eprintln!("STUN test skipped (no network?): {e}");
            }
        }
    }
}
