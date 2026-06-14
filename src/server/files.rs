//! File access (the printer's storage) — list + upload, behind a seam so the API
//! is testable without a printer: tests and `--fake` use [`FakeFiles`]; live mode
//! uses [`LiveFiles`] (FTPS). Listing is a read (open); upload is a write
//! (password-gated). Both are blocking — call from `spawn_blocking`.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::ResolvedTarget;
use crate::ftp::{FileEntry, FtpsClient};

/// Something that can list and upload files on the printer, and fetch the plate
/// preview embedded in a sliced `.3mf`.
pub trait FileStore: Send + Sync {
    /// List a directory (with dir/size info).
    fn list(&self, dir: &str) -> Result<Vec<FileEntry>, String>;
    fn upload(&self, remote_path: &str, bytes: Vec<u8>) -> Result<(), String>;
    /// The embedded plate preview PNG for a sliced `.3mf` (`Metadata/plate_N.png`),
    /// or `None` when the file has no such thumbnail.
    fn thumbnail(&self, remote_path: &str, plate: u32) -> Result<Option<Vec<u8>>, String>;
}

/// Extract `Metadata/plate_{plate}.png` (the slicer's plate preview) from a
/// `.3mf` (a zip). Returns `None` if the entry is absent/empty.
fn extract_thumbnail(zip_bytes: &[u8], plate: u32) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).map_err(|e| e.to_string())?;
    for name in [
        format!("Metadata/plate_{plate}.png"),
        format!("Metadata/plate_{plate}_small.png"),
    ] {
        if let Ok(mut entry) = archive.by_name(&name) {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            if !buf.is_empty() {
                return Ok(Some(buf));
            }
        }
    }
    Ok(None)
}

/// Real printer storage over implicit FTPS.
pub struct LiveFiles {
    target: ResolvedTarget,
    /// Cache of extracted thumbnails (key `path#plate`). Pulling a preview means
    /// downloading the whole `.3mf` over FTPS, so cache it — otherwise repeated
    /// list renders re-download every file and thumbnails flicker / time out.
    thumb_cache: Mutex<HashMap<String, Option<Vec<u8>>>>,
}

impl LiveFiles {
    pub fn new(target: ResolvedTarget) -> Self {
        Self {
            target,
            thumb_cache: Mutex::new(HashMap::new()),
        }
    }
}

impl FileStore for LiveFiles {
    fn list(&self, dir: &str) -> Result<Vec<FileEntry>, String> {
        FtpsClient::new(self.target.clone())
            .list_entries(dir)
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

    fn thumbnail(&self, remote_path: &str, plate: u32) -> Result<Option<Vec<u8>>, String> {
        let key = format!("{remote_path}#{plate}");
        // Recover from a poisoned lock rather than panicking every later request.
        if let Some(hit) = self
            .thumb_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(hit.clone());
        }
        let tmp = tempfile::Builder::new()
            .prefix("bambu-thumb-")
            .tempfile()
            .map_err(|e| e.to_string())?;
        FtpsClient::new(self.target.clone())
            .download(remote_path, tmp.path())
            .map_err(|e| e.to_string())?;
        let bytes = std::fs::read(tmp.path()).map_err(|e| e.to_string())?;
        let thumb = extract_thumbnail(&bytes, plate)?;
        let mut cache = self.thumb_cache.lock().unwrap_or_else(|e| e.into_inner());
        // Bound the cache — keys are caller-controlled on an open endpoint.
        if cache.len() >= 128 {
            cache.clear();
        }
        cache.insert(key, thumb.clone());
        Ok(thumb)
    }
}

/// A canned file store for `--fake` mode and tests.
pub struct FakeFiles;

impl FileStore for FakeFiles {
    fn list(&self, _dir: &str) -> Result<Vec<FileEntry>, String> {
        let entry = |name: &str, is_dir: bool, size: u64| FileEntry {
            name: name.to_string(),
            is_dir,
            size,
        };
        Ok(vec![
            entry("cache", true, 0),
            entry("timelapse", true, 0),
            entry("coin2c.gcode.3mf", false, 184_320),
            entry("benchy_2c.3mf", false, 256_000),
        ])
    }
    fn upload(&self, _remote_path: &str, _bytes: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
    fn thumbnail(&self, _remote_path: &str, _plate: u32) -> Result<Option<Vec<u8>>, String> {
        // A tiny valid PNG so the UI/E2E has something to render in --fake mode.
        Ok(Some(TINY_PNG.to_vec()))
    }
}

/// A 1×1 PNG (the `--fake` placeholder preview).
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xdd, 0x8d, 0xb0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plate_thumbnail_from_a_3mf_zip() {
        // Build a minimal zip with Metadata/plate_1.png.
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zw.start_file::<_, ()>("Metadata/plate_1.png", zip::write::FileOptions::default())
                .unwrap();
            zw.write_all(TINY_PNG).unwrap();
            zw.finish().unwrap();
        }
        assert_eq!(
            extract_thumbnail(&buf, 1).unwrap().as_deref(),
            Some(TINY_PNG)
        );
        assert_eq!(extract_thumbnail(&buf, 2).unwrap(), None); // no plate 2
    }
}
