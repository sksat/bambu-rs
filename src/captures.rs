//! Listing finished/in-progress capture runs on disk — the reusable read side of the
//! timelapse feature, so the CLI and the server present the same recordings.
//!
//! A serve capture writes to `captures/<epoch>_<name>_<mode>/<camera-id>/`, where each
//! camera dir holds one of: `park_NNNNNN.jpg` (a park-detected clean timelapse),
//! `frame_NNNNNN_*.jpg` (a printer-synced smooth timelapse), or `plain.mp4` / `plain.mjpeg`
//! (a head-in-shot video). The *kind* is detected from the files, not the dir name (older
//! runs lacked the `_<mode>` suffix), so it stays correct across layout changes.
//!
//! The filename → kind classification is pure (unit-tested); walking the directory is the
//! thin I/O wrapper (tested with a temp tree).

use std::path::Path;

use serde::Serialize;

/// What a camera's capture dir holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureKind {
    /// Object-only timelapse, park-detected from the camera (`park_*.jpg`). Scrubbable.
    Park,
    /// Object-only timelapse, printer-layer-synced snapshots (`frame_*.jpg`).
    Smooth,
    /// A head-in-shot video (`plain.mp4` / `plain.mjpeg`).
    Video,
}

/// One camera's output within a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureCam {
    /// The camera id (its subdir name; empty for an old single-dir layout).
    pub id: String,
    pub kind: CaptureKind,
    /// Frame count for an image-sequence kind (Park/Smooth); 0 for a Video.
    pub frames: u64,
    /// Whether an assembled/recorded `plain.mp4` is already present (Video only).
    pub has_mp4: bool,
}

/// One capture run (a print's recordings), newest first when listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureRun {
    /// The run dir name, e.g. `1781634785_cube-petg2_gcode_3mf`.
    pub id: String,
    /// Unix epoch parsed from the dir name prefix (0 if unparseable).
    pub started_at: u64,
    /// The human-ish remainder of the dir name (the sanitized job name).
    pub label: String,
    pub cameras: Vec<CaptureCam>,
}

/// Classify a camera dir from its filenames (pure). `None` when nothing recognizable is
/// present (e.g. an empty dir or a transient `.ring`).
pub fn classify(files: &[String]) -> Option<CaptureCam> {
    let count = |prefix: &str| {
        files
            .iter()
            .filter(|f| f.starts_with(prefix) && f.ends_with(".jpg"))
            .count() as u64
    };
    let has = |name: &str| files.iter().any(|f| f == name);
    let parks = count("park_");
    let frames = count("frame_");
    let has_mp4 = has("plain.mp4");
    let has_mjpeg = has("plain.mjpeg");
    if parks > 0 {
        Some(CaptureCam {
            id: String::new(),
            kind: CaptureKind::Park,
            frames: parks,
            has_mp4: false,
        })
    } else if frames > 0 {
        Some(CaptureCam {
            id: String::new(),
            kind: CaptureKind::Smooth,
            frames,
            has_mp4: false,
        })
    } else if has_mp4 || has_mjpeg {
        Some(CaptureCam {
            id: String::new(),
            kind: CaptureKind::Video,
            frames: 0,
            has_mp4,
        })
    } else {
        None
    }
}

/// Split a run dir name into (epoch, label): `1781634785_cube_gcode_3mf` → (1781634785,
/// "cube_gcode_3mf"). A non-numeric/absent prefix yields `(0, whole-name)`.
pub fn parse_run_id(id: &str) -> (u64, String) {
    match id.split_once('_') {
        Some((epoch, rest)) => match epoch.parse::<u64>() {
            Ok(e) => (e, rest.to_string()),
            Err(_) => (0, id.to_string()),
        },
        None => (0, id.to_string()),
    }
}

/// Read the filenames (not subdirs) directly in `dir`.
fn file_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// List capture runs under `root` (newest first). Each run's cameras are its per-camera
/// subdirs that hold recognizable output; an older single-dir run (files directly in the
/// run dir) surfaces as one camera with an empty id. Best-effort: unreadable dirs are
/// skipped, never an error.
pub fn list_captures(root: &Path) -> Vec<CaptureRun> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut runs: Vec<CaptureRun> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|run_entry| {
            let run_dir = run_entry.path();
            let id = run_entry.file_name().into_string().ok()?;
            let mut cameras = Vec::new();
            // Per-camera subdirs.
            if let Ok(inner) = std::fs::read_dir(&run_dir) {
                for sub in inner.flatten() {
                    if !sub.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    if let (Some(mut cam), Ok(cam_id)) = (
                        classify(&file_names(&sub.path())),
                        sub.file_name().into_string(),
                    ) {
                        cam.id = cam_id;
                        cameras.push(cam);
                    }
                }
            }
            // Old layout: files directly in the run dir. Give it a non-empty id so it has
            // a usable download URL; the endpoint maps a missing subdir back to the run dir.
            if let Some(mut cam) = classify(&file_names(&run_dir)) {
                cam.id = "default".to_string();
                cameras.push(cam);
            }
            if cameras.is_empty() {
                return None;
            }
            cameras.sort_by(|a, b| a.id.cmp(&b.id));
            let (started_at, label) = parse_run_id(&id);
            Some(CaptureRun {
                id,
                started_at,
                label,
                cameras,
            })
        })
        .collect();
    runs.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
    runs
}

/// ffmpeg argv (after the program name) to assemble a camera dir's image sequence into
/// `out` at `fps`. `None` for a Video kind (it's already a file — nothing to assemble).
/// Park frames are a contiguous `park_%06d.jpg`; smooth frames sort lexicographically, so
/// glob them. Pure, so the command shape is unit-tested without ffmpeg.
pub fn assemble_args(
    cam_dir: &Path,
    kind: CaptureKind,
    out: &Path,
    fps: u32,
) -> Option<Vec<String>> {
    let fps = fps.max(1).to_string();
    // Downscale huge frames + yuv420p so the mp4 plays everywhere.
    let vf = "scale='min(1280,iw)':-2,format=yuv420p".to_string();
    let mut args = vec!["-y".into(), "-framerate".into(), fps];
    match kind {
        CaptureKind::Park => {
            args.extend([
                "-start_number".into(),
                "0".into(),
                "-i".into(),
                cam_dir.join("park_%06d.jpg").display().to_string(),
            ]);
        }
        CaptureKind::Smooth => {
            args.extend([
                "-pattern_type".into(),
                "glob".into(),
                "-i".into(),
                cam_dir.join("frame_*.jpg").display().to_string(),
            ]);
        }
        CaptureKind::Video => return None,
    }
    args.extend([
        "-vf".into(),
        vf,
        "-c:v".into(),
        "libx264".into(),
        "-crf".into(),
        "23".into(),
        "-movflags".into(),
        "+faststart".into(),
        out.display().to_string(),
    ]);
    Some(args)
}

/// Assemble a camera dir's image sequence to an mp4 at `out` (overwriting). The thin ffmpeg
/// seam shared by the CLI and the server. Returns a friendly error if ffmpeg is missing or
/// fails, or if the kind isn't an image sequence.
pub fn assemble_mp4(cam_dir: &Path, kind: CaptureKind, out: &Path, fps: u32) -> Result<(), String> {
    let Some(args) = assemble_args(cam_dir, kind, out, fps) else {
        return Err("this recording is already a video — nothing to assemble".to_string());
    };
    let status = std::process::Command::new("ffmpeg")
        .args(&args)
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg not found on PATH — install ffmpeg to assemble the mp4".to_string()
            } else {
                format!("running ffmpeg: {e}")
            }
        })?;
    if !status.success() {
        return Err(format!("ffmpeg failed to assemble {}", out.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn classify_detects_each_kind() {
        assert_eq!(
            classify(&s(&["park_000000.jpg", "park_000001.jpg", "parks.jsonl"])),
            Some(CaptureCam {
                id: String::new(),
                kind: CaptureKind::Park,
                frames: 2,
                has_mp4: false
            })
        );
        assert_eq!(
            classify(&s(&[
                "frame_000001_layer_00057.jpg",
                "frame_000002_layer_00000.jpg"
            ]))
            .map(|c| (c.kind, c.frames)),
            Some((CaptureKind::Smooth, 2))
        );
        assert_eq!(
            classify(&s(&["plain.mp4"])).map(|c| (c.kind, c.has_mp4)),
            Some((CaptureKind::Video, true))
        );
        assert_eq!(
            classify(&s(&["plain.mjpeg"])).map(|c| (c.kind, c.has_mp4)),
            Some((CaptureKind::Video, false))
        );
        assert_eq!(classify(&s(&["notes.txt"])), None);
        assert_eq!(classify(&s(&[])), None);
    }

    #[test]
    fn parse_run_id_splits_epoch_and_label() {
        assert_eq!(
            parse_run_id("1781634785_cube_gcode_3mf"),
            (1781634785, "cube_gcode_3mf".to_string())
        );
        assert_eq!(parse_run_id("noepoch"), (0, "noepoch".to_string()));
        assert_eq!(parse_run_id("x_y"), (0, "x_y".to_string()));
    }

    #[test]
    fn lists_runs_newest_first_with_cameras() {
        let root = std::env::temp_dir().join(format!("bambu-caps-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // run A (older): one park camera
        let a = root.join("100_old_print").join("ext-0");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("park_000000.jpg"), b"x").unwrap();
        fs::write(a.join("park_000001.jpg"), b"x").unwrap();
        // run B (newer): a smooth camera + a video camera
        let b0 = root.join("200_new_print").join("ext-0");
        let b1 = root.join("200_new_print").join("ext-1");
        fs::create_dir_all(&b0).unwrap();
        fs::create_dir_all(&b1).unwrap();
        fs::write(b0.join("frame_000001_layer_00001.jpg"), b"x").unwrap();
        fs::write(b1.join("plain.mp4"), b"x").unwrap();

        let runs = list_captures(&root);
        assert_eq!(runs.len(), 2);
        // newest (epoch 200) first
        assert_eq!(runs[0].id, "200_new_print");
        assert_eq!(runs[0].started_at, 200);
        assert_eq!(runs[0].cameras.len(), 2);
        assert_eq!(runs[0].cameras[0].id, "ext-0");
        assert_eq!(runs[0].cameras[0].kind, CaptureKind::Smooth);
        assert_eq!(runs[0].cameras[1].kind, CaptureKind::Video);
        assert!(runs[0].cameras[1].has_mp4);
        // older run
        assert_eq!(runs[1].id, "100_old_print");
        assert_eq!(runs[1].cameras[0].kind, CaptureKind::Park);
        assert_eq!(runs[1].cameras[0].frames, 2);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn old_single_dir_layout_surfaces_as_one_camera() {
        let root = std::env::temp_dir().join(format!("bambu-caps-old-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let run = root.join("50_legacy");
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join("frame_000001_layer_00057.jpg"), b"x").unwrap();
        let runs = list_captures(&root);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].cameras.len(), 1);
        assert_eq!(runs[0].cameras[0].id, "default"); // files directly in the run dir
        assert_eq!(runs[0].cameras[0].kind, CaptureKind::Smooth);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_root_is_empty_not_an_error() {
        assert!(list_captures(Path::new("/no/such/captures/dir")).is_empty());
    }

    #[test]
    fn assemble_args_match_the_kind() {
        let dir = Path::new("/caps/run/ext-0");
        let out = Path::new("/caps/run/ext-0/timelapse.mp4");
        let park = assemble_args(dir, CaptureKind::Park, out, 10)
            .unwrap()
            .join(" ");
        assert!(park.contains("-framerate 10"), "{park}");
        assert!(
            park.contains("-start_number 0 -i /caps/run/ext-0/park_%06d.jpg"),
            "{park}"
        );
        assert!(park.contains("libx264"), "{park}");
        assert!(
            park.trim_end().ends_with("/caps/run/ext-0/timelapse.mp4"),
            "{park}"
        );

        let smooth = assemble_args(dir, CaptureKind::Smooth, out, 12)
            .unwrap()
            .join(" ");
        assert!(
            smooth.contains("-pattern_type glob -i /caps/run/ext-0/frame_*.jpg"),
            "{smooth}"
        );

        // A video has nothing to assemble.
        assert!(assemble_args(dir, CaptureKind::Video, out, 10).is_none());
    }
}
