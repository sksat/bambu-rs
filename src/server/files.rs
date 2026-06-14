//! File access (the printer's storage) — list + upload, behind a seam so the API
//! is testable without a printer: tests and `--fake` use [`FakeFiles`]; live mode
//! uses [`LiveFiles`] (FTPS). Listing is a read (open); upload is a write
//! (password-gated). Both are blocking — call from `spawn_blocking`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::ResolvedTarget;
use crate::ftp::{FileEntry, FtpsClient};

/// How long a directory listing stays fresh before re-fetching over FTPS. Bounds
/// the per-poll FTPS connect load (the UI auto-refreshes); writes invalidate it.
const LIST_TTL: Duration = Duration::from_secs(6);

/// Something that can list and upload files on the printer, and fetch the plate
/// preview embedded in a sliced `.3mf`.
pub trait FileStore: Send + Sync {
    /// List a directory (with dir/size info).
    fn list(&self, dir: &str) -> Result<Vec<FileEntry>, String>;
    /// Upload the already-staged `local` file to `remote_path`.
    fn upload(&self, remote_path: &str, local: &Path) -> Result<(), String>;
    /// The embedded plate preview PNG for a sliced `.3mf` (`Metadata/plate_N.png`),
    /// or `None` when the file has no such thumbnail.
    fn thumbnail(&self, remote_path: &str, plate: u32) -> Result<Option<Vec<u8>>, String>;
    /// Fetch a file's raw bytes (for the 3D viewer). Capped at [`RAW_MAX`].
    fn fetch(&self, remote_path: &str) -> Result<Vec<u8>, String>;
    /// Extract the plate's gcode (`Metadata/plate_N.gcode`) from a `.3mf` — the
    /// toolpath the 3D viewer renders for a sliced file. `None` if absent.
    fn gcode(&self, remote_path: &str, plate: u32) -> Result<Option<Vec<u8>>, String>;
}

/// Extract `Metadata/plate_{plate}.gcode` (the sliced toolpath) from a `.3mf`.
fn extract_gcode(zip_bytes: &[u8], plate: u32) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).map_err(|e| e.to_string())?;
    let mut entry = match archive.by_name(&format!("Metadata/plate_{plate}.gcode")) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok((!buf.is_empty()).then_some(buf))
}

/// Cap for [`FileStore::fetch`] — the whole file is buffered for the viewer.
pub const RAW_MAX: u64 = 64 * 1024 * 1024;

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
    /// Short-TTL cache of directory listings (key = dir), to bound FTPS connects
    /// under the UI's auto-refresh.
    list_cache: Mutex<HashMap<String, (Instant, Vec<FileEntry>)>>,
    /// Cache of extracted plate gcode (key `path#plate`). Like thumbnails, this
    /// means downloading the whole `.3mf` over FTPS, so cache it; only modestly
    /// sized toolpaths are kept (see [`GCODE_CACHE_MAX`]).
    gcode_cache: Mutex<HashMap<String, Option<Vec<u8>>>>,
}

/// Don't cache plate gcode larger than this (a big print's toolpath is many MB;
/// the viewer only opens one at a time, so the win is re-opens, not memory).
const GCODE_CACHE_MAX: usize = 8 * 1024 * 1024;

impl LiveFiles {
    pub fn new(target: ResolvedTarget) -> Self {
        Self {
            target,
            thumb_cache: Mutex::new(HashMap::new()),
            list_cache: Mutex::new(HashMap::new()),
            gcode_cache: Mutex::new(HashMap::new()),
        }
    }
}

impl FileStore for LiveFiles {
    fn list(&self, dir: &str) -> Result<Vec<FileEntry>, String> {
        if let Some((at, entries)) = self
            .list_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(dir)
            && at.elapsed() < LIST_TTL
        {
            return Ok(entries.clone());
        }
        let entries = FtpsClient::new(self.target.clone())
            .list_entries(dir)
            .map_err(|e| e.to_string())?;
        let mut cache = self.list_cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(dir.to_string(), (Instant::now(), entries.clone()));
        Ok(entries)
    }

    fn upload(&self, remote_path: &str, local: &Path) -> Result<(), String> {
        FtpsClient::new(self.target.clone())
            .upload(local, remote_path)
            .map(|_| ())
            .map_err(|e| e.to_string())?;
        // The directory changed — drop cached listings so the new file shows.
        self.list_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Ok(())
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

    fn fetch(&self, remote_path: &str) -> Result<Vec<u8>, String> {
        let tmp = tempfile::Builder::new()
            .prefix("bambu-raw-")
            .tempfile()
            .map_err(|e| e.to_string())?;
        FtpsClient::new(self.target.clone())
            .download(remote_path, tmp.path())
            .map_err(|e| e.to_string())?;
        let meta = std::fs::metadata(tmp.path()).map_err(|e| e.to_string())?;
        if meta.len() > RAW_MAX {
            return Err(format!("file too large to view ({} bytes)", meta.len()));
        }
        std::fs::read(tmp.path()).map_err(|e| e.to_string())
    }

    fn gcode(&self, remote_path: &str, plate: u32) -> Result<Option<Vec<u8>>, String> {
        let key = format!("{remote_path}#{plate}");
        if let Some(hit) = self
            .gcode_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(hit.clone());
        }
        let tmp = tempfile::Builder::new()
            .prefix("bambu-gcode-")
            .tempfile()
            .map_err(|e| e.to_string())?;
        FtpsClient::new(self.target.clone())
            .download(remote_path, tmp.path())
            .map_err(|e| e.to_string())?;
        let bytes = std::fs::read(tmp.path()).map_err(|e| e.to_string())?;
        let gcode = extract_gcode(&bytes, plate)?;
        // Only cache modest toolpaths (keys are caller-controlled on an open
        // endpoint); always bound the entry count.
        if gcode.as_ref().is_none_or(|g| g.len() <= GCODE_CACHE_MAX) {
            let mut cache = self.gcode_cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.len() >= 16 {
                cache.clear();
            }
            cache.insert(key, gcode.clone());
        }
        Ok(gcode)
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
    fn upload(&self, _remote_path: &str, _local: &Path) -> Result<(), String> {
        Ok(())
    }
    fn thumbnail(&self, _remote_path: &str, _plate: u32) -> Result<Option<Vec<u8>>, String> {
        // A tiny valid PNG so the UI/E2E has something to render in --fake mode.
        Ok(Some(TINY_PNG.to_vec()))
    }
    fn fetch(&self, _remote_path: &str) -> Result<Vec<u8>, String> {
        // Not a real 3mf; the viewer surfaces a load error in --fake mode.
        Ok(b"fake-model".to_vec())
    }
    fn gcode(&self, _remote_path: &str, _plate: u32) -> Result<Option<Vec<u8>>, String> {
        // A tiny sample toolpath (a few stacked square perimeters) so the viewer
        // and E2E render a real, non-empty model in --fake mode.
        Ok(Some(fake_gcode().into_bytes()))
    }
}

/// A small valid gcode toolpath: 6 layers of a 20 mm square perimeter, with
/// extrusion moves so [`GCodeLoader`] draws extruded (not travel) segments.
fn fake_gcode() -> String {
    let mut s = String::from("; fake sample toolpath\nG21\nG90\nM82\n");
    let mut e = 0.0_f64;
    for layer in 0..6 {
        let z = 0.2 * (layer as f64 + 1.0);
        s.push_str(&format!("G1 Z{z:.2} F600\nG1 X10 Y10 F3000\n"));
        for &(x, y) in &[(30.0, 10.0), (30.0, 30.0), (10.0, 30.0), (10.0, 10.0)] {
            e += 1.0;
            s.push_str(&format!("G1 X{x:.1} Y{y:.1} E{e:.3} F1200\n"));
        }
    }
    s
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

    #[test]
    fn extracts_plate_gcode_from_a_3mf_zip() {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zw.start_file::<_, ()>("Metadata/plate_1.gcode", zip::write::FileOptions::default())
                .unwrap();
            zw.write_all(b"G1 X0 Y0\nG1 X10 Y10 E1\n").unwrap();
            zw.finish().unwrap();
        }
        assert!(
            extract_gcode(&buf, 1)
                .unwrap()
                .unwrap()
                .starts_with(b"G1 X0 Y0")
        );
        assert_eq!(extract_gcode(&buf, 2).unwrap(), None); // no plate 2
    }

    #[test]
    fn fake_gcode_is_parseable_extruding_toolpath() {
        let g = String::from_utf8(fake_gcode().into_bytes()).unwrap();
        assert!(g.contains("G1 X30.0 Y10.0 E"));
    }
}
