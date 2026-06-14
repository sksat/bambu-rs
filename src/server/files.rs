//! File access (the printer's storage) — list + upload, behind a seam so the API
//! is testable without a printer: tests and `--fake` use [`FakeFiles`]; live mode
//! uses [`LiveFiles`] (FTPS). Listing is a read (open); upload is a write
//! (password-gated). Both are blocking — call from `spawn_blocking`.

use crate::config::ResolvedTarget;
use crate::ftp::FtpsClient;

/// Something that can list and upload files on the printer.
pub trait FileStore: Send + Sync {
    fn list(&self, dir: &str) -> Result<Vec<String>, String>;
    fn upload(&self, remote_path: &str, bytes: Vec<u8>) -> Result<(), String>;
}

/// Real printer storage over implicit FTPS.
pub struct LiveFiles {
    target: ResolvedTarget,
}

impl LiveFiles {
    pub fn new(target: ResolvedTarget) -> Self {
        Self { target }
    }
}

impl FileStore for LiveFiles {
    fn list(&self, dir: &str) -> Result<Vec<String>, String> {
        FtpsClient::new(self.target.clone())
            .list(dir)
            .map_err(|e| e.to_string())
    }

    fn upload(&self, remote_path: &str, bytes: Vec<u8>) -> Result<(), String> {
        // FtpsClient uploads from a path (atomic .part rename), so stage the bytes
        // in a temp file first.
        let tmp = tempfile::Builder::new()
            .prefix("bambu-upload-")
            .tempfile()
            .map_err(|e| e.to_string())?;
        std::fs::write(tmp.path(), &bytes).map_err(|e| e.to_string())?;
        FtpsClient::new(self.target.clone())
            .upload(tmp.path(), remote_path)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// A canned file store for `--fake` mode and tests.
pub struct FakeFiles;

impl FileStore for FakeFiles {
    fn list(&self, _dir: &str) -> Result<Vec<String>, String> {
        Ok(vec![
            "coin2c.gcode.3mf".to_string(),
            "benchy_2c.3mf".to_string(),
        ])
    }
    fn upload(&self, _remote_path: &str, _bytes: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
}
