//! Camera snapshot — the A1/P1 chamber-image stream.
//!
//! Proprietary JPEG stream over **TLS on TCP 6000** (see `docs/protocol.md`):
//! send an 80-byte auth packet (`bblp` + access code), then read framed JPEGs
//! (16-byte header whose first u32 is the JPEG length). Self-signed v1 cert, so
//! native-tls with accept-invalid-certs (same as FTPS).
//!
//! Note: the A1 camera is intermittently unavailable (a firmware quirk); when it
//! isn't streaming, the connection is accepted but no frame arrives, surfaced
//! here as a read timeout.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use native_tls::TlsConnector;

use crate::config::ResolvedTarget;

const CAMERA_PORT: u16 = 6000;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors from a camera snapshot. Messages never include the access code.
#[derive(Debug, thiserror::Error)]
pub enum CameraError {
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error("camera connection failed: {0}")]
    Connect(String),
    #[error("camera i/o error (no frame? the A1 camera is often off): {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected frame header (jpeg length {0}); framing differs or camera off")]
    BadFrame(u32),
}

/// The 80-byte auth packet (header + 32-byte user + 32-byte access code).
fn auth_packet(access_code: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(80);
    p.extend_from_slice(&0x40u32.to_le_bytes());
    p.extend_from_slice(&0x3000u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    let mut field = [0u8; 32];
    let user = b"bblp";
    field[..user.len()].copy_from_slice(user);
    p.extend_from_slice(&field);
    let mut field = [0u8; 32];
    let code = access_code.as_bytes();
    let n = code.len().min(32);
    field[..n].copy_from_slice(&code[..n]);
    p.extend_from_slice(&field);
    p
}

/// A one-shot camera client.
pub struct CameraClient {
    target: ResolvedTarget,
    timeout: Duration,
}

impl CameraClient {
    pub fn new(target: ResolvedTarget) -> Self {
        Self {
            target,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Grab a single JPEG frame from the chamber camera.
    pub fn snapshot(&self) -> Result<Vec<u8>, CameraError> {
        let connector = TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .map_err(|e| CameraError::Tls(e.to_string()))?;
        let tcp = TcpStream::connect((self.target.ip.as_str(), CAMERA_PORT))?;
        tcp.set_read_timeout(Some(self.timeout))?;
        tcp.set_write_timeout(Some(self.timeout))?;
        let mut tls = connector
            .connect(&self.target.ip, tcp)
            .map_err(|e| CameraError::Connect(e.to_string()))?;

        tls.write_all(&auth_packet(&self.target.access_code))?;

        let mut header = [0u8; 16];
        tls.read_exact(&mut header)?;
        let jpeg_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        if !(1000..=8_000_000).contains(&jpeg_len) {
            return Err(CameraError::BadFrame(jpeg_len));
        }
        let mut jpeg = vec![0u8; jpeg_len as usize];
        tls.read_exact(&mut jpeg)?;
        Ok(jpeg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_packet_layout() {
        let p = auth_packet("12345678");
        assert_eq!(p.len(), 80);
        assert_eq!(&p[0..4], &0x40u32.to_le_bytes()); // payload size
        assert_eq!(&p[4..8], &0x3000u32.to_le_bytes()); // type
        assert_eq!(&p[16..20], b"bblp"); // username, null-padded
        assert_eq!(p[20], 0);
        assert_eq!(&p[48..56], b"12345678"); // access code at offset 48
    }
}
