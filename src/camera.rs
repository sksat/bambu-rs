//! Camera snapshot — the A1/P1 chamber-image stream.
//!
//! Proprietary JPEG stream over **TLS on TCP 6000** (see `docs/protocol.md`):
//! send an 80-byte auth packet (`bblp` + access code), then read framed JPEGs
//! (16-byte header whose first u32 is the JPEG length). Self-signed v1 cert, so it
//! shares the LAN rustls config ([`crate::tls`]) that accepts any certificate.
//!
//! Note: the A1 camera is intermittently unavailable (a firmware quirk); when it
//! isn't streaming, the connection is accepted but no frame arrives, surfaced
//! here as a read timeout.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};

use crate::config::ResolvedTarget;
use crate::core::camerad::{self, FRAME_HEADER};

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
        let config =
            crate::tls::lan_client_config().map_err(|e| CameraError::Tls(e.to_string()))?;
        let tcp = TcpStream::connect((self.target.ip.as_str(), self.target.camera_port))?;
        tcp.set_read_timeout(Some(self.timeout))?;
        tcp.set_write_timeout(Some(self.timeout))?;
        let server_name = ServerName::try_from(self.target.ip.clone())
            .map_err(|e| CameraError::Tls(e.to_string()))?;
        let conn = ClientConnection::new(config, server_name)
            .map_err(|e| CameraError::Tls(e.to_string()))?;
        let mut tls = StreamOwned::new(conn, tcp);

        tls.write_all(&camerad::auth_packet(&self.target.access_code))?;

        let mut header = [0u8; FRAME_HEADER];
        tls.read_exact(&mut header)?;
        let jpeg_len = camerad::frame_len(&header).map_err(|e| match e {
            camerad::CameraError::FrameSize(n) => CameraError::BadFrame(n),
            other => CameraError::Tls(other.to_string()),
        })?;
        let mut jpeg = vec![0u8; jpeg_len];
        tls.read_exact(&mut jpeg)?;
        Ok(jpeg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_and_the_emulated_printer_share_one_wire_format() {
        // The layout itself is pinned in `core::camerad`. What matters here is
        // that this client reads it from there rather than keeping a private
        // copy that could drift away from what the relay serves.
        let p = camerad::auth_packet("12345678");
        assert_eq!(p.len(), camerad::AUTH_PACKET);
        assert_eq!(&p[48..56], b"12345678");
        let creds = camerad::parse_auth(&p).unwrap().unwrap();
        assert_eq!(creds.access_code, "12345678");
    }
}
