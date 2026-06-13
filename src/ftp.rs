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
    /// in memory.
    pub fn download(&self, remote_path: &str, local: &Path) -> Result<u64, FtpError> {
        let mut file = std::fs::File::create(local)?;
        let mut ftp = self.connect()?;
        let result = ftp
            .retr(remote_path, |reader| {
                // The closure must return suppaftp's FtpResult; wrap any local
                // write error as a connection error so RETR is finalised cleanly.
                std::io::copy(reader, &mut file).map_err(suppaftp::FtpError::ConnectionError)
            })
            .map_err(|e| FtpError::Ftp(e.to_string()));
        let _ = ftp.quit();
        result
    }

    /// Delete `remote_path` on the printer (FTP `DELE`).
    pub fn delete(&self, remote_path: &str) -> Result<(), FtpError> {
        let mut ftp = self.connect()?;
        let result = ftp.rm(remote_path).map_err(|e| FtpError::Ftp(e.to_string()));
        let _ = ftp.quit();
        result
    }
}
