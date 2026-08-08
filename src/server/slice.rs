//! Slicing a model into a printable `.gcode.3mf`, behind a seam like
//! [`Starter`](super::start::Starter): tests and `--fake` use [`FakeSlicer`],
//! live mode uses [`LiveSlicer`] (a discovered OrcaSlicer / Bambu Studio
//! binary), and a host with neither gets [`NoSlicer`] so `serve` still starts
//! and the API degrades with a reason instead of 500ing.
//!
//! All the decisions live in [`crate::core::slice`]; this module is the I/O
//! around them — finding the binary, reading profile JSON off disk, spawning the
//! process, and owning the one job slot.
//!
//! **Verification is not the slicer's job.** [`SliceManager`] re-reads whatever
//! any `Slicer` produced and runs `core::slice::verify_sliced` on it, so a
//! `FakeSlicer` that emitted a generic off-bed start gcode would fail CI on the
//! same code path production uses.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::core::slice::{self, ProfileFetch, ProfileKind, ProfileMap};

/// Everything one slice needs. Built by the handler from the request plus the
/// printer's capability row; nothing here has a server-side default.
#[derive(Debug, Clone)]
pub struct SliceParams {
    /// The staged model file on disk.
    pub input: PathBuf,
    /// Its original filename (extension included) — what the slicer is told to
    /// read is `input`, but this is what the user called it.
    pub input_name: String,
    /// Per-job working directory: the flattened profiles, the slicer log and the
    /// output all land here and die with the job.
    pub out_dir: PathBuf,
    /// Bare output filename, e.g. `cube.gcode.3mf`.
    pub out_name: String,
    /// Full machine preset, e.g. `Bambu Lab A1 mini 0.4 nozzle`.
    pub machine_profile: String,
    /// Process/filament preset suffix, e.g. `@BBL A1M`.
    pub preset_suffix: String,
    pub layer_mm: f64,
    /// Full filament preset name, e.g. `Bambu PLA Basic @BBL A1M`.
    pub filament: String,
    /// e.g. `Textured PEI Plate`. Drives the bed temperature — there is no
    /// default, because guessing it is the Cool-Plate-at-35°C mistake.
    pub bed_type: String,
    pub brim_mm: Option<f64>,
}

/// What kind of slicer is installed, and what it can do here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlicerInfo {
    /// `orca-slicer` / `bambu-studio` / `fake`, or the binary's own name.
    pub kind: String,
    /// Whether a virtual display is available. Without one the slice still
    /// succeeds; the 3mf just ships with no preview PNG, so a dashboard shows a
    /// blank thumbnail.
    pub thumbnails: bool,
}

/// A produced slice, plus the bed it was sliced for (so verification can check
/// the start gcode against the machine's real bounds rather than a guess).
#[derive(Debug, Clone)]
pub struct SliceOutput {
    pub path: PathBuf,
    pub bed: Option<(f64, f64)>,
}

/// Slices models. Blocking — call from `spawn_blocking`, same contract as
/// [`Starter`](super::start::Starter).
pub trait Slicer: Send + Sync {
    /// `Err(reason)` = slicing is unavailable on this host, with a
    /// human-readable explanation the API hands back verbatim.
    fn info(&self) -> Result<SlicerInfo, String>;
    fn slice(&self, p: &SliceParams) -> Result<SliceOutput, String>;
}

// ── Profile lookup (the only place profiles touch the filesystem) ───────────

/// Reads profiles out of a slicer's `.../profiles/BBL` bundle.
pub struct FsProfileFetch {
    root: PathBuf,
}

impl FsProfileFetch {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl ProfileFetch for FsProfileFetch {
    fn fetch(&self, kind: ProfileKind, name: &str) -> Result<Option<ProfileMap>, String> {
        // Defence in depth. The handler already rejects unsafe names, and
        // `is_safe_profile_name` forbids both separators — but this is the
        // function that turns a request string into a path, so it checks too,
        // and then proves the resolved path really did stay under the bundle
        // (a symlink inside it could still point out).
        if !slice::is_safe_profile_name(name) {
            return Err(format!("unsafe profile name {name:?}"));
        }
        let dir = self.root.join(kind.subdir());
        let path = dir.join(format!("{name}.json"));
        // Only *absent* may be a quiet `None`. Every other failure — EACCES,
        // ELOOP, ENOTDIR — is loud, because `core::slice` treats `None` for a
        // machine template as "this model has none", and a quiet skip there is
        // precisely how the generic off-bed start gcode gets emitted. An
        // unreadable profile must say so, not look like an absent one.
        let real = match path.canonicalize() {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("resolving {}: {e}", path.display())),
        };
        let real_dir = match dir.canonicalize() {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("resolving {}: {e}", dir.display())),
        };
        if !real.starts_with(&real_dir) {
            return Err(format!(
                "profile {name:?} resolves outside the profiles directory"
            ));
        }
        let bytes = match std::fs::read(&real) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("reading {}: {e}", real.display())),
        };
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("parsing {}: {e}", real.display()))?;
        match value {
            Value::Object(map) => Ok(Some(map)),
            _ => Err(format!("{} is not a JSON object", real.display())),
        }
    }
}

// ── Live slicer ────────────────────────────────────────────────────────────

/// What to tell someone who has no slicer installed.
const NO_SLICER: &str = "no slicer found: install orca-slicer (with its \
     /opt/orca-slicer/resources/profiles/BBL bundle) or Bambu Studio at \
     /opt/bambustudio-bin, or point `bambu serve` at one with --slicer-bin and \
     --slicer-profiles";

/// A real slicer binary plus its BBL profile bundle.
pub struct LiveSlicer {
    bin: PathBuf,
    profiles: PathBuf,
    kind: String,
    /// Extra environment the binary needs (Bambu Studio wants its bundled libs
    /// on `LD_LIBRARY_PATH` and `LC_ALL=C` — without the latter it segfaults).
    env: Vec<(String, String)>,
    xvfb: bool,
    timeout: Duration,
    /// Serialises the actual slicer processes across every printer this server
    /// serves. Two multi-gigabyte slicer processes would OOM a small host, and
    /// the second job is bounded anyway by its own timeout once it starts.
    run_lock: Mutex<()>,
}

impl LiveSlicer {
    /// Find a slicer, or explain why there isn't one.
    ///
    /// An explicitly-passed path is validated and never silently replaced by an
    /// auto-detected install — the flag exists precisely to choose *which*
    /// slicer runs.
    pub fn discover(
        bin: Option<PathBuf>,
        profiles: Option<PathBuf>,
        timeout: Duration,
    ) -> Result<Self, String> {
        let auto = auto_detect();
        let bin = match bin {
            Some(b) => {
                // Executable, not merely present: a non-executable file here
                // makes `GET /slice` advertise slicing and every job fail at
                // spawn. Same bar the PATH search applies.
                if !is_executable(&b) {
                    return Err(format!(
                        "--slicer-bin {} is not an executable file",
                        b.display()
                    ));
                }
                b
            }
            None => auto.clone().map(|(b, _)| b).ok_or(NO_SLICER)?,
        };
        let profiles = match profiles {
            Some(p) => {
                if !p.is_dir() {
                    return Err(format!(
                        "--slicer-profiles {} is not a directory",
                        p.display()
                    ));
                }
                p
            }
            // The bundle shipped beside the chosen binary — that install's own
            // profiles. Falling back to whatever `auto_detect` found would pair
            // one slicer's binary with another's profiles, which is how a
            // version mismatch becomes a silently wrong slice; only reuse it
            // when it belongs to the very binary selected.
            None => profiles_beside(&bin)
                .or_else(|| auto.filter(|(b, _)| *b == bin).map(|(_, p)| p))
                .ok_or_else(|| {
                    format!(
                        "no BBL profile bundle found beside {} — pass --slicer-profiles",
                        bin.display()
                    )
                })?,
        };
        for sub in ["machine", "process", "filament"] {
            if !profiles.join(sub).is_dir() {
                return Err(format!(
                    "{} is not a BBL profile bundle (no {sub}/ subdirectory)",
                    profiles.display()
                ));
            }
        }
        let (kind, env) = kind_and_env(&bin);
        Ok(Self {
            bin,
            profiles,
            kind,
            env,
            // Only affects the preview PNG; a headless box still slices.
            xvfb: which("xvfb-run").is_some(),
            timeout,
            run_lock: Mutex::new(()),
        })
    }
}

/// The first installed (binary, profiles) pair, in the order they were verified.
fn auto_detect() -> Option<(PathBuf, PathBuf)> {
    let orca =
        which("orca-slicer").map(|b| (b, PathBuf::from("/opt/orca-slicer/resources/profiles/BBL")));
    let studio = Some((
        PathBuf::from("/opt/bambustudio-bin/bin/bambu-studio"),
        PathBuf::from("/opt/bambustudio-bin/resources/profiles/BBL"),
    ));
    [orca, studio]
        .into_iter()
        .flatten()
        .find(|(b, p)| is_executable(b) && p.is_dir())
}

/// `<prefix>/bin/<slicer>` ships its profiles at `<prefix>/resources/profiles/BBL`.
fn profiles_beside(bin: &Path) -> Option<PathBuf> {
    let prefix = bin.parent()?.parent()?;
    let bundle = prefix.join("resources/profiles/BBL");
    bundle.is_dir().then_some(bundle)
}

/// How to name and run this binary. Bambu Studio is the one that needs help.
fn kind_and_env(bin: &Path) -> (String, Vec<(String, String)>) {
    let name = bin
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("slicer")
        .to_string();
    if name.starts_with("bambu-studio") {
        let libs = bin.parent().unwrap_or(Path::new("/")).display().to_string();
        return (
            name,
            vec![
                ("LD_LIBRARY_PATH".to_string(), libs),
                ("LC_ALL".to_string(), "C".to_string()),
            ],
        );
    }
    (name, Vec::new())
}

/// `which`, without a dependency: the first executable `name` on `$PATH`.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| is_executable(p))
    })
}

/// A file this process could actually run.
///
/// `is_file()` alone is not that: a non-executable `orca-slicer` on `$PATH`
/// would win auto-detection and then fail at spawn, instead of falling through
/// to the next candidate — an install that is present but unusable would report
/// "slicer found" and refuse every slice.
fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

impl Slicer for LiveSlicer {
    fn info(&self) -> Result<SlicerInfo, String> {
        Ok(SlicerInfo {
            kind: self.kind.clone(),
            thumbnails: self.xvfb,
        })
    }

    fn slice(&self, p: &SliceParams) -> Result<SliceOutput, String> {
        // A poisoned lock only means a previous slice panicked; the next one is
        // no less safe for it.
        let _run = self.run_lock.lock().unwrap_or_else(|e| e.into_inner());
        let fetch = FsProfileFetch::new(self.profiles.clone());

        // Which overrides a request produces, and that each is a STRING, is
        // decided in `core::slice` where a test can reach it.
        let (choice, overrides) =
            slice::overrides_for(p.layer_mm, &p.preset_suffix, &p.bed_type, p.brim_mm)?;

        let machine = slice::flatten(ProfileKind::Machine, &p.machine_profile, &fetch, &[])
            .map_err(|e| e.to_string())?;
        // Without bounds the off-bed check has nothing to compare against and
        // silently passes everything — the one invariant this whole feature
        // exists to enforce, quietly not enforced. A machine profile with no
        // usable `printable_area` is a broken bundle, not a slice to attempt.
        let bed = slice::bed_bounds(&machine).ok_or_else(|| {
            format!(
                "machine profile {:?} has no usable printable_area, so an off-bed \
                 prime line could not be detected",
                p.machine_profile
            )
        })?;
        let bed = Some(bed);
        let process = slice::flatten(ProfileKind::Process, &choice.name, &fetch, &overrides)
            .map_err(|e| e.to_string())?;
        let filament = slice::flatten(ProfileKind::Filament, &p.filament, &fetch, &[])
            .map_err(|e| e.to_string())?;

        // Into the per-job workdir, not a shared /tmp name: two printers slicing
        // at once must not read each other's settings.
        let machine_json = write_json(&p.out_dir, "machine.json", &machine)?;
        let process_json = write_json(&p.out_dir, "process.json", &process)?;
        let filament_json = write_json(&p.out_dir, "filament.json", &filament)?;

        let mut argv = vec![self.bin.display().to_string()];
        argv.extend(slice::slicer_args(
            &machine_json,
            &process_json,
            &filament_json,
            &p.out_dir,
            &p.out_name,
            &p.input,
        ));
        let argv = slice::wrap_xvfb(argv, self.xvfb);

        // Output goes to a log file rather than pipes: with a poll loop, a
        // chatty slicer would fill a pipe buffer and deadlock waiting for a
        // reader that is busy waiting for it.
        let log_path = p.out_dir.join("slicer.log");
        let log = std::fs::File::create(&log_path)
            .map_err(|e| format!("creating {}: {e}", log_path.display()))?;
        let log_err = log
            .try_clone()
            .map_err(|e| format!("duplicating the slicer log handle: {e}"))?;

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        #[cfg(unix)]
        {
            // Its own process group, so a timeout can kill `xvfb-run`'s Xvfb
            // too — killing only the child leaks one Xvfb per timeout.
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("{} not found — is the slicer still installed?", argv[0])
            } else {
                format!("running {}: {e}", argv[0])
            }
        })?;

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => return Err(format!("waiting for the slicer: {e}")),
            }
            if Instant::now() >= deadline {
                kill_group(&mut child);
                let _ = child.wait(); // reap, so the timeout leaves no zombie
                return Err(format!(
                    "slicer timed out after {}s (a GL-less host can wedge it; raise \
                     --slice-timeout-secs if the model is simply big)",
                    self.timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        };

        // The slicer reports failure by exit status; it also prints config
        // errors and keeps going far enough to look busy, so "exited 0" is not
        // on its own proof that a file exists.
        if !status.success() {
            return Err(format!(
                "{} exited {} — {}",
                self.kind,
                status
                    .code()
                    .map_or_else(|| "on a signal".to_string(), |c| c.to_string()),
                log_tail(&log_path)
            ));
        }
        let out = p.out_dir.join(&p.out_name);
        if !out.is_file() {
            return Err(format!(
                "the slicer exited 0 but produced no {} — {}",
                p.out_name,
                log_tail(&log_path)
            ));
        }
        Ok(SliceOutput { path: out, bed })
    }
}

/// Kill the slicer's whole process group (see the `process_group` call).
#[cfg(unix)]
fn kill_group(child: &mut std::process::Child) {
    let pid = child.id() as i32;
    // SAFETY: a plain `kill(2)`; a negative pid addresses the process group we
    // put the child in ourselves at spawn.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

/// The last part of the slicer log — what a human needs to see when it failed.
fn log_tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let tail: String = text
        .chars()
        .skip(text.chars().count().saturating_sub(1500))
        .collect();
    if tail.trim().is_empty() {
        "the slicer logged nothing".to_string()
    } else {
        tail.trim().to_string()
    }
}

fn write_json(dir: &Path, name: &str, map: &ProfileMap) -> Result<PathBuf, String> {
    let path = dir.join(name);
    let bytes = serde_json::to_vec(map).map_err(|e| format!("serialising {name}: {e}"))?;
    std::fs::write(&path, bytes).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path)
}

// ── Absent and fake slicers ────────────────────────────────────────────────

/// No slicer on this host (or none for this model). Carries the reason so the
/// API can say what to install instead of just refusing.
pub struct NoSlicer(pub String);

impl Slicer for NoSlicer {
    fn info(&self) -> Result<SlicerInfo, String> {
        Err(self.0.clone())
    }
    fn slice(&self, _p: &SliceParams) -> Result<SliceOutput, String> {
        Err(self.0.clone())
    }
}

/// A canned slicer for `--fake` mode and tests, so neither needs a slicer
/// installed.
///
/// Its output is deliberately *verifiable*: a real header carrying the requested
/// layer height and a start section that primes inside a 180×180 bed. The
/// manager verifies every slicer's output, fake included, so this cannot drift
/// into producing something production code would reject.
pub struct FakeSlicer;

impl Slicer for FakeSlicer {
    fn info(&self) -> Result<SlicerInfo, String> {
        Ok(SlicerInfo {
            kind: "fake".to_string(),
            thumbnails: false,
        })
    }

    fn slice(&self, p: &SliceParams) -> Result<SliceOutput, String> {
        let gcode = format!(
            "; HEADER_BLOCK_START\n\
             ; total layer number: 42\n\
             ; layer_height = {}\n\
             ; filament: {}\n\
             ; HEADER_BLOCK_END\n\
             M190 S65\n\
             M104 S220\n\
             G28\n\
             G1 Z5 F1200\n\
             G1 X2 Y185 F9000 ; out-of-bed wipe, no extrusion\n\
             G1 X2 Y10 F9000\n\
             G1 X2 Y150 E8 F1200 ; prime, on the bed\n\
             ; CHANGE_LAYER\n\
             G1 X60 Y60 E1 F1800\n",
            slice::fmt_mm(p.layer_mm),
            p.filament,
        );
        let md5 = crate::core::project::gcode_md5_hex(gcode.as_bytes());
        let plate_json =
            json!({ "bed_type": p.bed_type, "filament_colors": ["#00AE42"], "filament_ids": [0] })
                .to_string();

        let mut buf = Vec::new();
        {
            use zip::write::SimpleFileOptions;
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            let entries: [(&str, &[u8]); 3] = [
                ("Metadata/plate_1.gcode", gcode.as_bytes()),
                ("Metadata/plate_1.gcode.md5", md5.as_bytes()),
                ("Metadata/plate_1.json", plate_json.as_bytes()),
            ];
            for (name, bytes) in entries {
                zip.start_file(name, opts)
                    .and_then(|()| zip.write_all(bytes).map_err(Into::into))
                    .map_err(|e| format!("building the fake 3mf: {e}"))?;
            }
            zip.finish()
                .map_err(|e| format!("finishing the fake 3mf: {e}"))?;
        }
        let out = p.out_dir.join(&p.out_name);
        std::fs::write(&out, &buf).map_err(|e| format!("writing {}: {e}", out.display()))?;
        Ok(SliceOutput {
            path: out,
            // The fake impersonates an A1 mini everywhere else too.
            bed: Some((180.0, 180.0)),
        })
    }
}

// ── The one job slot ───────────────────────────────────────────────────────

/// The state of this printer's slice slot.
#[derive(Debug, Clone)]
pub struct SliceJobStatus {
    /// `idle` | `running` | `done` | `failed`.
    pub state: &'static str,
    pub input_name: Option<String>,
    pub out_name: Option<String>,
    pub layer_mm: Option<f64>,
    pub filament: Option<String>,
    pub bed_type: Option<String>,
    /// From the verified gcode, not from the request.
    pub layers: Option<u32>,
    pub bed_temp_c: Option<u32>,
    pub size_bytes: Option<u64>,
    pub error: Option<String>,
}

impl Default for SliceJobStatus {
    fn default() -> Self {
        Self {
            state: "idle",
            input_name: None,
            out_name: None,
            layer_mm: None,
            filament: None,
            bed_type: None,
            layers: None,
            bed_temp_c: None,
            size_bytes: None,
            error: None,
        }
    }
}

impl SliceJobStatus {
    /// Ad-hoc JSON, like `TimelapseStatus::to_json` — this is a poll shape for
    /// the dashboard, not part of the typed `PrinterStatus` contract, so it
    /// deliberately does not become a `ts-rs` export.
    pub fn to_json(&self) -> Value {
        json!({
            "state": self.state,
            "input_name": self.input_name,
            "out_name": self.out_name,
            "layer_mm": self.layer_mm,
            "filament": self.filament,
            "bed_type": self.bed_type,
            "layers": self.layers,
            "bed_temp_c": self.bed_temp_c,
            "size_bytes": self.size_bytes,
            "error": self.error,
        })
    }
}

#[derive(Default)]
struct Inner {
    /// Shared with the running task, which updates it; replaced on each start.
    status: Arc<Mutex<SliceJobStatus>>,
    handle: Option<tokio::task::JoinHandle<()>>,
    /// The job's working directory — flattened profiles, log, output. Held here
    /// so the finished output survives until the *next* job replaces it, and no
    /// longer: at most one workdir per printer exists at a time.
    workdir: Option<Arc<tempfile::TempDir>>,
    out_path: Option<PathBuf>,
}

/// One slice at a time, per printer.
///
/// A single slot rather than a job table: with no queue, no cancellation and no
/// persistence in scope, the dashboard only ever wants "the slice I just asked
/// for" — the same shape [`TimelapseManager`](super::timelapse::TimelapseManager)
/// settled on.
#[derive(Default)]
pub struct SliceManager {
    inner: Mutex<Inner>,
}

impl SliceManager {
    /// Begin a slice. Refuses while one is running; a finished job's working
    /// directory (and its output) is dropped as the new one takes the slot.
    pub fn start(
        &self,
        slicer: Arc<dyn Slicer>,
        params: SliceParams,
        staged: tempfile::NamedTempFile,
        workdir: tempfile::TempDir,
    ) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.status.lock().unwrap_or_else(|e| e.into_inner()).state == "running" {
            return Err("a slice is already running".to_string());
        }
        let out_path = workdir.path().join(&params.out_name);
        let status = Arc::new(Mutex::new(SliceJobStatus {
            state: "running",
            input_name: Some(params.input_name.clone()),
            out_name: Some(params.out_name.clone()),
            layer_mm: Some(params.layer_mm),
            filament: Some(params.filament.clone()),
            bed_type: Some(params.bed_type.clone()),
            ..Default::default()
        }));

        let shared = Arc::clone(&status);
        // The staged model is the run's, not the slot's: moving it into the task
        // means it is deleted when the slice ends rather than lingering until
        // the NEXT slice replaces it. A 512 MiB input sitting in /tmp until
        // someone happens to slice again is a bill nobody agreed to.
        let handle = tokio::spawn(async move {
            let want = params.layer_mm;
            let outcome =
                tokio::task::spawn_blocking(move || run_and_verify(slicer.as_ref(), &params, want))
                    .await;
            drop(staged);
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            match outcome {
                Ok(Ok((meta, size))) => {
                    s.state = "done";
                    s.layers = meta.layers;
                    s.bed_temp_c = meta.bed_temp_c;
                    s.size_bytes = Some(size);
                }
                Ok(Err(e)) => {
                    s.state = "failed";
                    s.error = Some(e);
                }
                Err(e) => {
                    s.state = "failed";
                    s.error = Some(format!("slice task failed: {e}"));
                }
            }
        });

        g.status = status;
        g.handle = Some(handle);
        g.workdir = Some(Arc::new(workdir)); // drops the previous job's workdir + output
        g.out_path = Some(out_path);
        Ok(())
    }

    pub fn status(&self) -> SliceJobStatus {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let s = g.status.lock().unwrap_or_else(|e| e.into_inner());
        s.clone()
    }

    /// The finished `.gcode.3mf` and the status that describes it, or `None`
    /// while idle/running/failed.
    ///
    /// The directory is handed back with the path, not just the path: the slot
    /// deletes the previous job's workdir the moment a new slice starts, and a
    /// download or an upload-and-print reading only a `PathBuf` would find the
    /// file gone mid-read. Holding the [`SliceResult`] keeps it until the
    /// caller is done with it.
    pub fn result(&self) -> Option<SliceResult> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let status = g.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if status.state != "done" {
            return None;
        }
        Some(SliceResult {
            path: g.out_path.clone()?,
            status,
            _workdir: g.workdir.clone()?,
        })
    }
}

/// A finished slice, borrowed for as long as the caller holds it.
pub struct SliceResult {
    pub path: PathBuf,
    pub status: SliceJobStatus,
    /// Kept only to keep the directory alive; a concurrent slice would
    /// otherwise delete it while this one is being read.
    _workdir: Arc<tempfile::TempDir>,
}

/// Slice, then prove the result — for **every** `Slicer`, live or fake.
///
/// A slicer that reports success having emitted the generic off-bed start gcode
/// (the trap this whole feature is built around) fails here, before anything can
/// be uploaded to a printer.
fn run_and_verify(
    slicer: &dyn Slicer,
    p: &SliceParams,
    want_layer_mm: f64,
) -> Result<(slice::SliceMeta, u64), String> {
    let out = slicer.slice(p)?;
    let bytes = std::fs::read(&out.path).map_err(|e| format!("reading the sliced 3mf: {e}"))?;
    let gcode = crate::core::project::plate_gcode(&bytes, 1)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            "the produced 3mf has no Metadata/plate_1.gcode — it was not sliced".to_string()
        })?;
    let meta = slice::verify_sliced(&gcode, want_layer_mm, out.bed)
        .map_err(|e| format!("verification failed: {e}"))?;
    Ok((meta, bytes.len() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wait until `cond` holds, failing only after a deadline no healthy job can
    /// reach. The manager drives a real background task, so "has it finished
    /// yet?" is genuinely a wall-clock question and a fixed sleep just guesses.
    async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
        const LIMIT: Duration = Duration::from_secs(20);
        let start = Instant::now();
        while !cond() {
            assert!(
                start.elapsed() < LIMIT,
                "timed out after {LIMIT:?} waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn params(dir: &Path, input: &Path, layer_mm: f64) -> SliceParams {
        SliceParams {
            input: input.to_path_buf(),
            input_name: "cube.stl".to_string(),
            out_dir: dir.to_path_buf(),
            out_name: "cube.gcode.3mf".to_string(),
            machine_profile: "Bambu Lab A1 mini 0.4 nozzle".to_string(),
            preset_suffix: "@BBL A1M".to_string(),
            layer_mm,
            filament: "Bambu PLA Basic @BBL A1M".to_string(),
            bed_type: "Textured PEI Plate".to_string(),
            brim_mm: None,
        }
    }

    fn staged() -> tempfile::NamedTempFile {
        tempfile::NamedTempFile::new().unwrap()
    }

    /// A slicer whose output is whatever the test says it is.
    type ScriptedSlice = Box<dyn Fn(&SliceParams) -> Result<SliceOutput, String> + Send + Sync>;
    struct ScriptedSlicer(ScriptedSlice);

    impl Slicer for ScriptedSlicer {
        fn info(&self) -> Result<SlicerInfo, String> {
            Ok(SlicerInfo {
                kind: "scripted".to_string(),
                thumbnails: false,
            })
        }
        fn slice(&self, p: &SliceParams) -> Result<SliceOutput, String> {
            (self.0)(p)
        }
    }

    #[tokio::test]
    async fn a_fake_slice_runs_verifies_and_exposes_its_result() {
        let mgr = SliceManager::default();
        assert_eq!(mgr.status().state, "idle");
        assert!(mgr.result().is_none());

        let work = tempfile::tempdir().unwrap();
        let input = staged();
        let p = params(work.path(), input.path(), 0.12);
        mgr.start(Arc::new(FakeSlicer), p, input, work).unwrap();

        eventually("the slice to finish", || mgr.status().state != "running").await;
        let s = mgr.status();
        assert_eq!(s.state, "done", "{:?}", s.error);
        // Read back out of the produced gcode, not echoed from the request.
        assert_eq!(s.layers, Some(42));
        assert_eq!(s.bed_temp_c, Some(65));
        assert!(s.size_bytes.unwrap() > 0);

        let done = mgr.result().unwrap();
        let (path, done) = (done.path.clone(), done.status.clone());
        assert_eq!(done.state, "done");
        assert!(path.is_file());
        // What comes out really is a sliced 3mf.
        let bytes = std::fs::read(&path).unwrap();
        assert!(crate::core::project::inspect_plate(&bytes, 1).is_ok());
    }

    #[tokio::test]
    async fn a_second_slice_is_refused_while_one_runs() {
        let mgr = SliceManager::default();
        // Blocks until released, so the slot is provably occupied.
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = Arc::clone(&gate);
        let slow = ScriptedSlicer(Box::new(move |_p| {
            while !seen.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err("released".to_string())
        }));

        let work = tempfile::tempdir().unwrap();
        let input = staged();
        let p = params(work.path(), input.path(), 0.2);
        mgr.start(Arc::new(slow), p, input, work).unwrap();
        eventually("the job to be running", || mgr.status().state == "running").await;

        let work2 = tempfile::tempdir().unwrap();
        let input2 = staged();
        let p2 = params(work2.path(), input2.path(), 0.2);
        let err = mgr
            .start(Arc::new(FakeSlicer), p2, input2, work2)
            .unwrap_err();
        assert!(err.contains("already running"), "{err}");
        // A refused start must not have taken the slot from the running job.
        assert!(mgr.result().is_none());

        gate.store(true, std::sync::atomic::Ordering::SeqCst);
        eventually("the job to fail", || mgr.status().state == "failed").await;
    }

    #[tokio::test]
    async fn a_slicer_that_fakes_a_good_slice_is_caught_by_verification() {
        // The point of verifying in the manager: this "slicer" exits happily
        // with the generic fallback start gcode, which drives off the bed.
        let mgr = SliceManager::default();
        let bad = ScriptedSlicer(Box::new(|p: &SliceParams| {
            let gcode = "; layer_height = 0.12\nG1 X10.1 Y200.0 E15 ;Draw the first line\n\
                         ; CHANGE_LAYER\n";
            let mut buf = Vec::new();
            {
                use zip::write::SimpleFileOptions;
                let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
                zip.start_file("Metadata/plate_1.gcode", SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(gcode.as_bytes()).unwrap();
                zip.finish().unwrap();
            }
            let out = p.out_dir.join(&p.out_name);
            std::fs::write(&out, &buf).unwrap();
            Ok(SliceOutput {
                path: out,
                bed: Some((180.0, 180.0)),
            })
        }));

        let work = tempfile::tempdir().unwrap();
        let input = staged();
        let p = params(work.path(), input.path(), 0.12);
        mgr.start(Arc::new(bad), p, input, work).unwrap();

        eventually("the slice to finish", || mgr.status().state != "running").await;
        let s = mgr.status();
        assert_eq!(s.state, "failed");
        let err = s.error.unwrap();
        assert!(err.contains("Draw the first line"), "{err}");
        assert!(
            mgr.result().is_none(),
            "a failed job has no result to print"
        );
    }

    #[tokio::test]
    async fn a_new_job_drops_the_previous_one_s_workdir() {
        let mgr = SliceManager::default();
        let work = tempfile::tempdir().unwrap();
        let old_out = work.path().join("cube.gcode.3mf");
        let input = staged();
        let p = params(work.path(), input.path(), 0.2);
        mgr.start(Arc::new(FakeSlicer), p, input, work).unwrap();
        eventually("the first slice to finish", || mgr.status().state == "done").await;
        assert!(old_out.is_file());

        let work2 = tempfile::tempdir().unwrap();
        let input2 = staged();
        let p2 = params(work2.path(), input2.path(), 0.16);
        mgr.start(Arc::new(FakeSlicer), p2, input2, work2).unwrap();
        // Bounded disk: at most one job's workdir per printer, ever.
        assert!(!old_out.exists(), "the finished job's output outlived it");
        eventually("the second slice to finish", || {
            mgr.status().state == "done"
        })
        .await;
        assert_eq!(mgr.status().layer_mm, Some(0.16));
    }

    // ── Profile lookup off a real directory ────────────────────────────────

    /// A minimal BBL-shaped bundle: a machine with a 180mm bed, one process, one
    /// filament.
    fn bundle() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let write = |sub: &str, name: &str, body: &str| {
            let d = dir.path().join(sub);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(format!("{name}.json")), body).unwrap();
        };
        write(
            "machine",
            "Test Machine 0.4 nozzle",
            r#"{"printable_area":["0x0","180x0","180x180","0x180"]}"#,
        );
        write(
            "process",
            "0.20mm Standard @TEST",
            r#"{"layer_height":"0.2"}"#,
        );
        write("filament", "Test PLA @TEST", r#"{"filament_type":["PLA"]}"#);
        dir
    }

    #[test]
    fn fs_profile_fetch_reads_a_profile_and_refuses_traversal() {
        let dir = bundle();
        let fetch = FsProfileFetch::new(dir.path().to_path_buf());
        let m = fetch
            .fetch(ProfileKind::Machine, "Test Machine 0.4 nozzle")
            .unwrap()
            .unwrap();
        assert_eq!(slice::bed_bounds(&m), Some((180.0, 180.0)));
        assert!(
            fetch
                .fetch(ProfileKind::Process, "no such profile")
                .unwrap()
                .is_none()
        );
        // The security boundary: a query parameter must not escape the bundle.
        let err = fetch
            .fetch(ProfileKind::Filament, "../../../../etc/passwd")
            .unwrap_err();
        assert!(err.contains("unsafe profile name"), "{err}");
    }

    // ── Unix-only from here: shell stubs, symlinks, permission bits and the
    // process-group kill. Gated as a group so the crate still compiles its
    // tests elsewhere; what they cover is the POSIX behaviour itself.
    #[cfg(unix)]
    mod posix {
        use super::*;

        #[test]
        fn discover_refuses_a_directory_that_is_not_a_profile_bundle() {
            let empty = tempfile::tempdir().unwrap();
            let bin = empty.path().join("slicer");
            std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
            // Present but not runnable: availability must not be advertised for an
            // install whose every job would fail at spawn.
            let err = LiveSlicer::discover(
                Some(bin.clone()),
                Some(empty.path().to_path_buf()),
                Duration::from_secs(1),
            )
            .err()
            .expect("a non-executable file is not a slicer");
            assert!(err.contains("not an executable file"), "{err}");

            std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            let err = LiveSlicer::discover(
                Some(bin.clone()),
                Some(empty.path().to_path_buf()),
                Duration::from_secs(1),
            )
            .err()
            .expect("an empty directory is not a profile bundle");
            assert!(err.contains("not a BBL profile bundle"), "{err}");

            // An explicit flag pointing at nothing is an error, never a silent
            // fallback to some other install.
            let err = LiveSlicer::discover(
                Some(empty.path().join("nope")),
                None,
                Duration::from_secs(1),
            )
            .err()
            .expect("an explicit --slicer-bin that does not exist is an error");
            assert!(err.contains("not an executable file"), "{err}");
        }

        /// A duration no other process on the machine is plausibly sleeping for,
        /// so the grandchild check can identify its own.
        const MARKER: &str = "31337";

        /// A stand-in slicer: it dispatches on the input extension exactly as
        /// libslic3r does, then emits a 3mf whose plate gcode verifies.
        const STUB_SLICER: &str = r#"#!/bin/sh
out=""; outdir=""; last=""
while [ $# -gt 0 ]; do
  case "$1" in
    --export-3mf) shift; out="$1" ;;
    --outputdir) shift; outdir="$1" ;;
  esac
  last="$1"; shift
done
case "$last" in
  *.stl|*.obj|*.3mf) ;;
  *) echo "$last: Unknown file format. Input file must have .stl, .obj, .amf(.xml) extension." >&2
     exit 6 ;;
esac
python3 - "$outdir/$out" <<PY
import sys, zipfile
with zipfile.ZipFile(sys.argv[1], "w") as z:
    z.writestr("Metadata/plate_1.gcode",
               "; layer_height = 0.2\nG28\nG1 X10 Y10 F3000\nM190 S65\n;LAYER_CHANGE\n")
PY
"#;

        #[test]
        fn the_whole_live_path_runs_from_a_request_to_a_verified_3mf() {
            // The composition — flatten, write the profile JSONs, build the argv,
            // spawn, verify — had no coverage past `spawn`, and that is exactly
            // where the bug lived that made every real slice fail: the staged model
            // was given no file extension, and the slicer picks its model reader
            // from one. A stub "slicer" that checks its own argv catches that class
            // without needing a real slicer on the machine.
            let dir = bundle();
            // A machine template sibling, so the merge that prevents the off-bed
            // start gcode is exercised rather than assumed.
            std::fs::write(
                dir.path()
                    .join("machine")
                    .join("Test Machine 0.4 nozzle template machine_start_gcode.json"),
                "{\"machine_start_gcode\":\"G28\\nG1 X10 Y10 F3000\\n\"}",
            )
            .unwrap();

            // Stands in for the slicer: refuses an extensionless input the way the
            // real one does, then writes a 3mf whose plate gcode passes verification.
            let script = dir.path().join("slicer.sh");
            std::fs::write(&script, STUB_SLICER).unwrap();
            std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();

            let mut slicer = LiveSlicer::discover(
                Some(script),
                Some(dir.path().to_path_buf()),
                Duration::from_secs(60),
            )
            .unwrap();
            slicer.xvfb = false;

            let work = tempfile::tempdir().unwrap();
            // Staged the way the handler stages it — extension and all.
            let input = tempfile::Builder::new()
                .prefix("bambu-model-")
                .suffix(".stl")
                .tempfile()
                .unwrap();
            std::fs::write(
                input.path(),
                b"solid t
endsolid t
",
            )
            .unwrap();
            let mut p = params(work.path(), input.path(), 0.2);
            p.machine_profile = "Test Machine 0.4 nozzle".to_string();
            p.preset_suffix = "@TEST".to_string();
            p.filament = "Test PLA @TEST".to_string();

            let (meta, size) = run_and_verify(&slicer, &p, 0.2).expect("a verified slice");
            assert_eq!(meta.layer_height_mm, 0.2);
            assert!(size > 0);
        }

        #[test]
        fn a_staged_model_without_an_extension_is_refused_by_the_slicer() {
            // The blocker, pinned: the same bytes under a name the slicer cannot
            // dispatch on fail before anything is read. Verified against the real
            // slicer on this host, and reproduced here by the stub.
            let dir = bundle();
            let script = dir.path().join("slicer.sh");
            std::fs::write(
                &script,
                "#!/bin/sh
for a; do last=$a; done
case \"$last\" in *.stl) exit 0;; esac
             echo \"$last: Unknown file format.\" >&2; exit 6
",
            )
            .unwrap();
            std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            let mut slicer = LiveSlicer::discover(
                Some(script),
                Some(dir.path().to_path_buf()),
                Duration::from_secs(60),
            )
            .unwrap();
            slicer.xvfb = false;
            let work = tempfile::tempdir().unwrap();
            let bare = tempfile::Builder::new()
                .prefix("bambu-model-")
                .tempfile()
                .unwrap();
            let mut p = params(work.path(), bare.path(), 0.2);
            p.machine_profile = "Test Machine 0.4 nozzle".to_string();
            p.preset_suffix = "@TEST".to_string();
            p.filament = "Test PLA @TEST".to_string();
            let err = slicer.slice(&p).unwrap_err();
            assert!(err.contains("Unknown file format"), "{err}");
        }

        #[test]
        fn a_symlink_out_of_the_bundle_is_refused_even_though_its_name_is_safe() {
            // The name filter cannot catch this: `escape` has no separator and no
            // dots, so it passes `is_safe_profile_name` and only the canonicalised
            // comparison stops it. Without a symlink no test reaches that branch,
            // which is why it was asserted by a comment rather than proven.
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.json"), br#"{"a":"1"}"#).unwrap();
            let dir = root.path().join("filament");
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(outside.path().join("secret.json"), dir.join("escape.json"))
                .unwrap();

            let f = FsProfileFetch::new(root.path().to_path_buf());
            assert!(
                slice::is_safe_profile_name("escape"),
                "the name is ordinary"
            );
            let err = f.fetch(ProfileKind::Filament, "escape").unwrap_err();
            assert!(err.contains("outside the profiles directory"), "{err}");
        }

        #[test]
        fn an_unreadable_profile_is_an_error_not_a_quiet_absence() {
            // `core::slice` reads `None` for a machine template as "this model has
            // none" and carries on — which is exactly how the generic off-bed start
            // gcode ships. Only *absent* may be quiet.
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("machine");
            std::fs::create_dir_all(&dir).unwrap();
            // A symlink to nowhere: canonicalize fails with ENOENT, which IS
            // absent, so this one is quiet…
            std::os::unix::fs::symlink(root.path().join("gone.json"), dir.join("dangling.json"))
                .unwrap();
            let f = FsProfileFetch::new(root.path().to_path_buf());
            assert!(f.fetch(ProfileKind::Machine, "dangling").unwrap().is_none());
            // …while a symlink loop is ELOOP, which is not.
            std::os::unix::fs::symlink(dir.join("loop_b.json"), dir.join("loop_a.json")).unwrap();
            std::os::unix::fs::symlink(dir.join("loop_a.json"), dir.join("loop_b.json")).unwrap();
            let err = f.fetch(ProfileKind::Machine, "loop_a").unwrap_err();
            assert!(err.contains("resolving"), "{err}");
        }

        #[test]
        fn a_wedged_slicer_is_killed_at_the_timeout_and_leaves_no_zombie() {
            use std::os::unix::fs::PermissionsExt;
            let dir = bundle();
            let script = dir.path().join("hang.sh");
            // A distinctive duration so `pgrep` finds THIS test's grandchild and
            // not some unrelated sleep on the machine.
            std::fs::write(&script, format!("#!/bin/sh\nsleep {MARKER}\n")).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

            let mut slicer = LiveSlicer::discover(
                Some(script),
                Some(dir.path().to_path_buf()),
                Duration::from_millis(300),
            )
            .unwrap();
            // Not under test here, and an xvfb-run on this host's PATH would change
            // which process we are timing.
            slicer.xvfb = false;

            let work = tempfile::tempdir().unwrap();
            let input = staged();
            let mut p = params(work.path(), input.path(), 0.2);
            p.machine_profile = "Test Machine 0.4 nozzle".to_string();
            p.preset_suffix = "@TEST".to_string();
            p.filament = "Test PLA @TEST".to_string();

            let started = Instant::now();
            let err = slicer.slice(&p).unwrap_err();
            assert!(err.contains("timed out after"), "{err}");
            // Killed, not merely abandoned: the call returns near the deadline and
            // the child was reaped (`wait` returned) before it did.
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{:?}",
                started.elapsed()
            );

            // The point of the process group, asserted rather than described: the
            // script is `sh` and the thing that sleeps is sh's CHILD, so killing
            // only the direct child leaves the grandchild running — one leaked
            // Xvfb (or slicer) per timeout. Reducing `kill_group` to
            // `child.kill()` passes every assertion above and fails this one.
            std::thread::sleep(Duration::from_millis(300));
            let survivors = std::process::Command::new("pgrep")
                .args(["-f", &format!("sleep {MARKER}")])
                .output()
                .expect("pgrep");
            let listed = String::from_utf8_lossy(&survivors.stdout);
            assert!(
                listed.trim().is_empty(),
                "the grandchild outlived the timeout: pids {}",
                listed.trim()
            );
        }
    }
}
