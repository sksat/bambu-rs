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

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::core::park::{SelectTuning, Selection, select_park_frame};

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

/// The representative still for a recording's thumbnail: the LAST frame of an image
/// sequence. A timelapse's final frame shows the most-built-up object, which reads best as
/// a poster. `None` for a Video (no frame file to serve — the caller extracts one with
/// ffmpeg, see [`video_thumb_args`]). Pure. Frame indices are zero-padded, so the
/// lexicographic max is the newest frame.
pub fn thumb_frame(files: &[String], kind: CaptureKind) -> Option<String> {
    let prefix = match kind {
        CaptureKind::Park => "park_",
        CaptureKind::Smooth => "frame_",
        CaptureKind::Video => return None,
    };
    files
        .iter()
        .filter(|f| f.starts_with(prefix) && f.ends_with(".jpg"))
        .max()
        .cloned()
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

/// The shared encode tail: downscale huge frames + yuv420p (plays everywhere), x264, and
/// faststart so the mp4 streams. Ends with the output path.
fn encode_tail(out: &Path) -> Vec<String> {
    vec![
        "-vf".into(),
        "scale='min(1280,iw)':-2,format=yuv420p".into(),
        "-c:v".into(),
        "libx264".into(),
        "-crf".into(),
        "23".into(),
        "-movflags".into(),
        "+faststart".into(),
        out.display().to_string(),
    ]
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
    let mut args = vec!["-y".into(), "-framerate".into(), fps.max(1).to_string()];
    match kind {
        CaptureKind::Park => args.extend([
            "-start_number".into(),
            "0".into(),
            "-i".into(),
            cam_dir.join("park_%06d.jpg").display().to_string(),
        ]),
        CaptureKind::Smooth => args.extend([
            "-pattern_type".into(),
            "glob".into(),
            "-i".into(),
            cam_dir.join("frame_*.jpg").display().to_string(),
        ]),
        CaptureKind::Video => return None,
    }
    args.extend(encode_tail(out));
    Some(args)
}

/// ffmpeg argv to transcode a raw `plain.mjpeg` (the fallback recorded when ffmpeg was
/// absent at capture time) into a playable mp4 at `out`. Pure.
pub fn transcode_args(input: &Path, out: &Path) -> Vec<String> {
    let mut args = vec!["-y".into(), "-i".into(), input.display().to_string()];
    args.extend(encode_tail(out));
    args
}

/// Transcode a raw mjpeg recording to mp4 (the thin ffmpeg seam). Same error mapping as
/// [`assemble_mp4`].
pub fn transcode_mp4(input: &Path, out: &Path) -> Result<(), String> {
    run_ffmpeg(&transcode_args(input, out), out)
}

/// ffmpeg argv to grab a single downscaled still from a video `input` into `out` (a jpeg
/// poster for a Video recording's thumbnail). Seeks ~1s in to skip a black/opening frame.
/// Pure, so the command shape is unit-tested without ffmpeg.
pub fn video_thumb_args(input: &Path, out: &Path) -> Vec<String> {
    vec![
        "-y".into(),
        "-ss".into(),
        "1".into(),
        "-i".into(),
        input.display().to_string(),
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        "scale='min(480,iw)':-2".into(),
        out.display().to_string(),
    ]
}

/// Extract a poster still from a video recording (the thin ffmpeg seam for Video thumbs).
pub fn extract_video_thumb(input: &Path, out: &Path) -> Result<(), String> {
    run_ffmpeg(&video_thumb_args(input, out), out)
}

/// Run ffmpeg with `args` producing `out`. The thin seam shared by assemble + transcode;
/// friendly error if ffmpeg is missing or the encode fails.
fn run_ffmpeg(args: &[String], out: &Path) -> Result<(), String> {
    let status = std::process::Command::new("ffmpeg")
        .args(args)
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg not found on PATH — install ffmpeg to make the mp4".to_string()
            } else {
                format!("running ffmpeg: {e}")
            }
        })?;
    if !status.success() {
        return Err(format!("ffmpeg failed to make {}", out.display()));
    }
    Ok(())
}

/// Assemble a camera dir's image sequence to an mp4 at `out` (overwriting). Shared by the
/// CLI's `--assemble` and the server. Errors if the kind isn't an image sequence.
pub fn assemble_mp4(cam_dir: &Path, kind: CaptureKind, out: &Path, fps: u32) -> Result<(), String> {
    let Some(args) = assemble_args(cam_dir, kind, out, fps) else {
        return Err("this recording is already a video — nothing to assemble".to_string());
    };
    run_ffmpeg(&args, out)
}

/// Decode size for smooth burst selection — tiny gray, matching the live park detector and
/// the skill's select_smooth.py (enough signal for the left-zone park, cheap to score).
pub const SMOOTH_DECODE_W: usize = 64;
pub const SMOOTH_DECODE_H: usize = 36;

/// Parse a smooth burst frame name `frame_<n>_layer_<L>_t<offset>.jpg` → `(layer, offset_ms)`.
/// `None` for anything that isn't a burst frame. Pure.
pub fn parse_smooth_frame(name: &str) -> Option<(u64, u64)> {
    let rest = name.strip_suffix(".jpg")?.strip_prefix("frame_")?;
    let (_n, rest) = rest.split_once("_layer_")?;
    let (layer, offset) = rest.split_once("_t")?;
    Some((layer.parse().ok()?, offset.parse().ok()?))
}

/// ffmpeg argv to decode every JPEG matching `glob` to a `w`×`h` gray rawvideo stream on
/// stdout (one frame after another, glob = capture order). One ffmpeg for the whole set —
/// no per-frame spawn. Pure (command shape unit-tested without ffmpeg).
pub fn gray_decode_args(glob: &Path, w: usize, h: usize) -> Vec<String> {
    vec![
        "-v".into(),
        "error".into(),
        "-f".into(),
        "image2".into(),
        "-pattern_type".into(),
        "glob".into(),
        "-i".into(),
        glob.display().to_string(),
        "-vf".into(),
        format!("scale={w}:{h},format=gray"),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "gray".into(),
        "-".into(),
    ]
}

/// Decode args for all `frame_*.jpg` in a smooth cam dir (whole-run selection).
pub fn smooth_decode_args(cam_dir: &Path, w: usize, h: usize) -> Vec<String> {
    gray_decode_args(&cam_dir.join("frame_*.jpg"), w, h)
}

/// Decode + select the parked frame from ONE layer's burst — the live path used during a
/// smooth capture (after each layer's burst settles). Decodes just that layer's
/// `frame_*_layer_<L>_t*.jpg` with one ffmpeg and runs [`select_park_frame`]. Returns the
/// chosen full-res JPEG path, or `None` if the layer's park wasn't captured.
pub fn select_layer_burst(
    cam_dir: &Path,
    layer: i64,
    sel: &SelectTuning,
) -> Result<Option<(PathBuf, f64)>, String> {
    let (w, h) = (SMOOTH_DECODE_W, SMOOTH_DECODE_H);
    let tag = format!("_layer_{layer:05}_t");
    let mut files: Vec<String> = std::fs::read_dir(cam_dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("frame_") && n.ends_with(".jpg") && n.contains(&tag))
        .collect();
    files.sort();
    if files.is_empty() {
        return Ok(None);
    }
    let glob = cam_dir.join(format!("frame_*{tag}*.jpg"));
    let out = std::process::Command::new("ffmpeg")
        .args(gray_decode_args(&glob, w, h))
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg not found on PATH — install ffmpeg".to_string()
            } else {
                format!("running ffmpeg: {e}")
            }
        })?;
    if !out.status.success() {
        return Err("ffmpeg failed to decode the layer burst".to_string());
    }
    let fsize = w * h;
    if out.stdout.len() != fsize * files.len() {
        return Err(format!(
            "decoded {} bytes, expected {} ({} frames)",
            out.stdout.len(),
            fsize * files.len(),
            files.len()
        ));
    }
    let mut frames = Vec::with_capacity(files.len());
    let mut paths = Vec::with_capacity(files.len());
    for (i, name) in files.iter().enumerate() {
        let Some((_l, offset)) = parse_smooth_frame(name) else {
            continue;
        };
        frames.push(crate::core::park::SelectFrame {
            offset_ms: offset,
            gray: out.stdout[i * fsize..(i + 1) * fsize].to_vec(),
        });
        paths.push((offset, cam_dir.join(name)));
    }
    match select_park_frame(&frames, w, h, sel) {
        Selection::Selected {
            offset_ms,
            confidence,
        } => Ok(paths
            .into_iter()
            .find(|(o, _)| *o == offset_ms)
            .map(|(_, p)| (p, confidence))),
        Selection::Skipped { .. } => Ok(None),
    }
}

/// Pick the parked frame from each layer's burst in a smooth cam dir, returning the chosen
/// full-res JPEG paths in layer order. Decodes the whole dir to tiny gray with ONE ffmpeg,
/// groups by layer, and runs the pure [`select_park_frame`]. Layers whose park fell outside
/// the burst are skipped (a gap beats a head-over-print frame).
pub fn select_smooth_frames(cam_dir: &Path, sel: &SelectTuning) -> Result<Vec<PathBuf>, String> {
    let (w, h) = (SMOOTH_DECODE_W, SMOOTH_DECODE_H);
    let mut files: Vec<String> = std::fs::read_dir(cam_dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("frame_") && n.ends_with(".jpg"))
        .collect();
    files.sort(); // lexicographic = zero-padded frame index = ffmpeg glob order
    if files.is_empty() {
        return Err("no smooth frames to select".to_string());
    }
    let out = std::process::Command::new("ffmpeg")
        .args(smooth_decode_args(cam_dir, w, h))
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg not found on PATH — install ffmpeg".to_string()
            } else {
                format!("running ffmpeg: {e}")
            }
        })?;
    if !out.status.success() {
        return Err("ffmpeg failed to decode smooth frames".to_string());
    }
    let fsize = w * h;
    if out.stdout.len() != fsize * files.len() {
        // glob/sort drift or a decode hiccup — bail so the caller falls back to assemble-all.
        return Err(format!(
            "decoded {} bytes, expected {} ({} frames)",
            out.stdout.len(),
            fsize * files.len(),
            files.len()
        ));
    }
    // Group frames by layer (BTreeMap keeps layer order); keep each frame's offset→path.
    use std::collections::BTreeMap;
    type LayerGroup = (Vec<crate::core::park::SelectFrame>, Vec<(u64, PathBuf)>);
    let mut by_layer: BTreeMap<u64, LayerGroup> = BTreeMap::new();
    for (i, name) in files.iter().enumerate() {
        let Some((layer, offset)) = parse_smooth_frame(name) else {
            continue;
        };
        let gray = out.stdout[i * fsize..(i + 1) * fsize].to_vec();
        let entry = by_layer.entry(layer).or_default();
        entry.0.push(crate::core::park::SelectFrame {
            offset_ms: offset,
            gray,
        });
        entry.1.push((offset, cam_dir.join(name)));
    }
    let mut selected = Vec::new();
    for (_layer, (frames, paths)) in by_layer {
        if let Selection::Selected { offset_ms, .. } = select_park_frame(&frames, w, h, sel)
            && let Some((_, path)) = paths.into_iter().find(|(o, _)| *o == offset_ms)
        {
            selected.push(path);
        }
    }
    Ok(selected)
}

/// Assemble a specific, ordered list of full-res JPEGs into an mp4 (the selected parked
/// frames). Stages them as a contiguous `sel_%06d.jpg` sequence in a temp subdir next to
/// `out`, image2-assembles, then cleans up.
pub fn assemble_selected_mp4(frames: &[PathBuf], out: &Path, fps: u32) -> Result<(), String> {
    if frames.is_empty() {
        return Err("no selected frames to assemble".to_string());
    }
    let stage = out
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".sel-stage");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
    for (i, f) in frames.iter().enumerate() {
        std::fs::copy(f, stage.join(format!("sel_{i:06}.jpg"))).map_err(|e| e.to_string())?;
    }
    let mut args = vec![
        "-y".into(),
        "-framerate".into(),
        fps.max(1).to_string(),
        "-start_number".into(),
        "0".into(),
        "-i".into(),
        stage.join("sel_%06d.jpg").display().to_string(),
    ];
    args.extend(encode_tail(out));
    let r = run_ffmpeg(&args, out);
    let _ = std::fs::remove_dir_all(&stage);
    r
}

/// The clean smooth timelapse: select the parked frame per layer, then assemble just those.
/// Errors (and the caller falls back to the raw all-frames assemble) if nothing selectable.
pub fn assemble_smooth_selected_mp4(
    cam_dir: &Path,
    sel: &SelectTuning,
    out: &Path,
    fps: u32,
) -> Result<(), String> {
    let frames = select_smooth_frames(cam_dir, sel)?;
    if frames.is_empty() {
        return Err("no parked frames selected".to_string());
    }
    assemble_selected_mp4(&frames, out, fps)
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
    fn thumb_frame_picks_the_last_frame_of_a_sequence() {
        // Park/Smooth → the highest-numbered (most-built-up) frame; order in the listing
        // doesn't matter since indices are zero-padded.
        assert_eq!(
            thumb_frame(
                &s(&[
                    "park_000002.jpg",
                    "park_000000.jpg",
                    "park_000001.jpg",
                    "parks.jsonl"
                ]),
                CaptureKind::Park
            ),
            Some("park_000002.jpg".to_string())
        );
        assert_eq!(
            thumb_frame(
                &s(&[
                    "frame_000001_layer_00057.jpg",
                    "frame_000012_layer_00120.jpg"
                ]),
                CaptureKind::Smooth
            ),
            Some("frame_000012_layer_00120.jpg".to_string())
        );
        // A Video has no frame file to serve; the caller extracts one with ffmpeg.
        assert_eq!(thumb_frame(&s(&["plain.mp4"]), CaptureKind::Video), None);
        // Nothing matching → None (no crash on an empty/odd dir).
        assert_eq!(thumb_frame(&s(&["notes.txt"]), CaptureKind::Park), None);
    }

    #[test]
    fn video_thumb_args_grab_one_downscaled_still() {
        let input = Path::new("/caps/run/ext-1/plain.mp4");
        let out = Path::new("/caps/run/ext-1/thumb.jpg");
        let a = video_thumb_args(input, out).join(" ");
        assert!(a.contains("-i /caps/run/ext-1/plain.mp4"), "{a}");
        assert!(a.contains("-frames:v 1"), "{a}");
        assert!(a.contains("scale="), "{a}");
        assert!(a.trim_end().ends_with("/caps/run/ext-1/thumb.jpg"), "{a}");
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

    #[test]
    fn parse_smooth_frame_pulls_layer_and_offset() {
        assert_eq!(
            parse_smooth_frame("frame_000012_layer_00007_t0900.jpg"),
            Some((7, 900))
        );
        assert_eq!(
            parse_smooth_frame("frame_000001_layer_00000_t0100.jpg"),
            Some((0, 100))
        );
        // Not a burst frame.
        assert_eq!(parse_smooth_frame("park_000003.jpg"), None);
        assert_eq!(parse_smooth_frame("plain.mp4"), None);
    }

    #[test]
    fn smooth_decode_args_glob_to_gray_rawvideo() {
        let a = smooth_decode_args(Path::new("/caps/run/ext-0"), 64, 36).join(" ");
        assert!(
            a.contains("-pattern_type glob -i /caps/run/ext-0/frame_*.jpg"),
            "{a}"
        );
        assert!(a.contains("scale=64:36,format=gray"), "{a}");
        assert!(a.contains("-f rawvideo"), "{a}");
        assert!(a.trim_end().ends_with(" -"), "{a}");
    }

    #[test]
    fn transcode_args_wrap_an_mjpeg_input() {
        let input = Path::new("/caps/run/ext-1/plain.mjpeg");
        let out = Path::new("/caps/run/ext-1/plain.mp4");
        let a = transcode_args(input, out).join(" ");
        assert!(a.contains("-i /caps/run/ext-1/plain.mjpeg"), "{a}");
        assert!(a.contains("libx264"), "{a}");
        assert!(a.trim_end().ends_with("/caps/run/ext-1/plain.mp4"), "{a}");
    }
}
