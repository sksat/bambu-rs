//! FTPS file transfer — the printer's LAN file store.
//!
//! **Implicit** FTPS on port 990, same `bblp` + access-code auth as MQTT, and
//! the same self-signed X.509 **v1** certificate. So this uses native-tls
//! (OpenSSL) with accept-invalid-certs — the rustls path rejects v1 certs (see
//! `client.rs` for the same issue) — the equivalent of OpenSSL `CERT_NONE`,
//! acceptable only for this LAN-direct self-signed case.

use std::path::Path;

use native_tls::TlsConnector;
use suppaftp::{NativeTlsConnector, NativeTlsFtpStream};

use crate::config::ResolvedTarget;

const FTPS_PORT: u16 = 990;
const FTP_USER: &str = "bblp";

/// Errors from FTPS operations. Messages never include the access code.
#[derive(Debug, thiserror::Error)]
pub enum FtpError {
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error("FTP error: {0}")]
    Ftp(String),
    #[error("local file error: {0}")]
    Io(#[from] std::io::Error),
}

/// The temp path a download streams to before the atomic rename: `<local>.part`
/// (same directory, so the rename stays on one filesystem and is atomic).
fn part_path(local: &Path) -> std::path::PathBuf {
    let mut name = local.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    local.with_file_name(name)
}

/// A one-shot FTPS client (connect → act → quit per call).
pub struct FtpsClient {
    target: ResolvedTarget,
}

impl FtpsClient {
    pub fn new(target: ResolvedTarget) -> Self {
        Self { target }
    }

    fn connect(&self) -> Result<NativeTlsFtpStream, FtpError> {
        let connector = TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .map_err(|e| FtpError::Tls(e.to_string()))?;
        let mut ftp = NativeTlsFtpStream::connect_secure_implicit(
            (self.target.ip.as_str(), FTPS_PORT),
            NativeTlsConnector::from(connector),
            &self.target.ip,
        )
        .map_err(|e| FtpError::Ftp(e.to_string()))?;
        ftp.login(FTP_USER, &self.target.access_code)
            .map_err(|e| FtpError::Ftp(e.to_string()))?;
        Ok(ftp)
    }

    /// List file names in `dir` (FTP `NLST`).
    pub fn list(&self, dir: &str) -> Result<Vec<String>, FtpError> {
        let mut ftp = self.connect()?;
        let names = ftp
            .nlst(Some(dir))
            .map_err(|e| FtpError::Ftp(e.to_string()))?;
        let _ = ftp.quit();
        Ok(names)
    }

    /// Upload a local file to `remote_path` on the printer; returns bytes sent.
    pub fn upload(&self, local: &Path, remote_path: &str) -> Result<u64, FtpError> {
        let mut file = std::fs::File::open(local)?;
        let mut ftp = self.connect()?;
        let result = ftp
            .put_file(remote_path, &mut file)
            .map_err(|e| FtpError::Ftp(e.to_string()));
        let _ = ftp.quit();
        result
    }

    /// Download `remote_path` from the printer to `local`; returns bytes written.
    /// Streams (FTP `RETR`) so large files (e.g. timelapse videos) aren't held
    /// in memory. Writes to a sibling temp file and atomically renames on
    /// success, so a failed or partial transfer never clobbers an existing
    /// destination.
    pub fn download(&self, remote_path: &str, local: &Path) -> Result<u64, FtpError> {
        let tmp = part_path(local);
        let mut file = std::fs::File::create(&tmp)?;
        let mut ftp = self.connect()?;
        let result = ftp
            .retr(remote_path, |reader| {
                // The closure must return suppaftp's FtpResult; wrap any local
                // write error as a connection error so RETR is finalised cleanly.
                std::io::copy(reader, &mut file).map_err(suppaftp::FtpError::ConnectionError)
            })
            .map_err(|e| FtpError::Ftp(e.to_string()));
        let _ = ftp.quit();
        drop(file);
        match result {
            Ok(n) => {
                std::fs::rename(&tmp, local)?;
                Ok(n)
            }
            Err(e) => {
                // Leave no partial file behind on failure.
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Delete `remote_path` on the printer (FTP `DELE`).
    pub fn delete(&self, remote_path: &str) -> Result<(), FtpError> {
        let mut ftp = self.connect()?;
        let result = ftp.rm(remote_path).map_err(|e| FtpError::Ftp(e.to_string()));
        let _ = ftp.quit();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_path_is_a_sibling_with_a_part_suffix() {
        assert_eq!(part_path(Path::new("/tmp/v.mp4")), Path::new("/tmp/v.mp4.part"));
        assert_eq!(part_path(Path::new("out.jpg")), Path::new("out.jpg.part"));
    }
}
