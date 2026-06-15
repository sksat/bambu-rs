//! The `bambu` command-line interface (behind the `cli` feature).
//!
//! Thin layer over the library: parse args, resolve a connection target, call
//! the client, format output. Agent contract: human-readable output by default,
//! machine-readable JSON to stdout with `--json` (no TTY auto-detection — output
//! format depends only on the flag); a semantic exit-code scheme; the access
//! code is never printed.

use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::camera::{CameraClient, CameraError};
use crate::client::{
    ClientError, CommandOutcome, LanMqttClient, StatusSource, VerifyStage, WatchStep,
};
use crate::config::{self, Config, ConfigError, Overrides, Profile, ResolvedTarget};
use crate::core::capability::{self, ControlAssessment, ControlRefusal};
use crate::core::command::{
    AmsControl, AmsFilamentSetting, Command as ProtoCommand, LedNode, ProjectFile, SpeedLevel,
    TimelapseControl,
};
use crate::core::project::{self, PlateInspection};
use crate::core::report::ReportState;
use crate::core::safety::{self, GcodeVerdict, TempLimits};
use crate::core::stage::Stage;
use crate::core::status::{GcodeState, PrinterStatus};
use crate::core::version::Module;
use crate::ftp::{FtpError, FtpsClient};

/// Exit codes (a subset of the documented scheme).
mod exit {
    pub const GENERAL: u8 = 1;
    pub const VALIDATION: u8 = 3;
    pub const CONFIRM_REQUIRED: u8 = 4;
    pub const PRINTER_BUSY: u8 = 5;
    pub const VERIFY_TIMEOUT: u8 = 6;
    pub const TRANSPORT: u8 = 7;
    pub const DEVICE_REJECTED: u8 = 8;
}

#[derive(Parser)]
#[command(
    name = "bambu",
    version,
    about = "Monitor and drive Bambu Lab printers over the LAN"
)]
struct Cli {
    /// Printer profile to use (defaults to the configured default).
    #[arg(long, global = true)]
    printer: Option<String>,
    /// Override the printer IP address.
    #[arg(long, global = true)]
    ip: Option<String>,
    /// Override the serial number.
    #[arg(long, global = true)]
    serial: Option<String>,
    /// Override the LAN access code.
    #[arg(long, global = true)]
    access_code: Option<String>,
    /// Override the model (e.g. a1mini).
    #[arg(long, global = true)]
    model: Option<String>,
    /// Emit machine-readable JSON (default output is human-readable).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage saved printer profiles.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Print a status snapshot; with --watch, monitor continuously.
    Status {
        /// Continuously monitor: print live updates and do NOT stop at job
        /// completion (runs until --timeout or Ctrl-C). To watch a print *to
        /// completion*, use `job start --watch`.
        #[arg(long)]
        watch: bool,
        /// With --watch, poll every N seconds (sends `pushall`) for a higher
        /// data rate, like Bambu Studio. Default: passive (printer's ~2s push).
        #[arg(long)]
        interval: Option<u64>,
        /// With --watch, give up only after NO report for this many seconds
        /// (resets while the printer responds; drops auto-reconnect). Default 2m.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Show the printer's firmware/module inventory and resolved capabilities.
    Info,
    /// Decode the active HMS (Health Management System) alerts.
    Hms,
    /// Start, pause, resume or stop a print job.
    Job {
        #[command(subcommand)]
        action: JobAction,
    },
    /// Transfer files to/from the printer over FTPS.
    File {
        #[command(subcommand)]
        action: FileAction,
    },
    /// Camera operations (A1/P1 chamber-image stream).
    Camera {
        #[command(subcommand)]
        action: CameraAction,
    },
    /// Timelapse: toggle printer-side recording, fetch videos, or drive an
    /// external camera from the print's own layer events.
    Timelapse {
        #[command(subcommand)]
        action: TimelapseAction,
    },
    /// Turn a light on or off (control test; low-risk).
    Light {
        /// "on" or "off".
        #[arg(value_parser = ["on", "off"])]
        state: String,
        /// Which light: chamber (default) or work. `work` is [spec] — not every
        /// model has one (this A1 mini only reports `chamber_light`).
        #[arg(long, default_value = "chamber", value_parser = ["chamber", "work"])]
        node: String,
        /// Watch the report for this many seconds after sending.
        #[arg(long, default_value_t = 8)]
        timeout: u64,
    },
    /// Set the print-speed profile (can be sent mid-print; reversible).
    Speed {
        /// Speed level.
        #[arg(value_parser = ["silent", "standard", "sport", "ludicrous"])]
        level: String,
        /// Watch the report for this many seconds to confirm spd_lvl changed.
        #[arg(long, default_value_t = 8)]
        timeout: u64,
    },
    /// AMS operations (control, filament change, tray settings). [spec] —
    /// derived from OpenBambuAPI, not yet confirmed on this unit's AMS Lite.
    Ams {
        #[command(subcommand)]
        action: AmsAction,
    },
    /// Run printer calibration — pick what to calibrate (a subcommand).
    Calibrate {
        #[command(subcommand)]
        what: CalibrateAction,
    },
    /// Send a raw G-code line and watch the report (control; needs --confirm).
    Gcode {
        /// The G-code line, e.g. "G28" (home all axes).
        line: String,
        /// Required to actually send a control command.
        #[arg(long)]
        confirm: bool,
        /// Override the static safety check (over-limit temps, cold extrusion).
        #[arg(long)]
        force: bool,
        /// Watch the report for this many seconds after sending.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Reboot the printer (disruptive; needs --confirm). The printer drops the
    /// connection and restarts (~1–2 min) and may rejoin DHCP on a new IP.
    Reboot {
        /// Required — the printer will disconnect and restart.
        #[arg(long)]
        confirm: bool,
    },
    /// Serve the monitoring + control HTTP API (and the web dashboard SPA when
    /// built with the `dashboard` feature).
    #[cfg(feature = "server")]
    #[command(alias = "dashboard")]
    Serve {
        /// Bind host. Default 127.0.0.1; a non-loopback host serves over the
        /// network (without --password, control is open — a warning is printed).
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Bind port.
        #[arg(long, default_value_t = 8088)]
        port: u16,
        /// Password gating control (write) requests. Reads are always open; if
        /// omitted, control is open too. May also be set via $BAMBU_SERVE_PASSWORD.
        #[arg(long, env = "BAMBU_SERVE_PASSWORD")]
        password: Option<String>,
        /// Serve deterministic fake data (no printer needed; for demos/E2E).
        #[arg(long)]
        fake: bool,
        /// Poll the printer every N seconds for live updates (default: passive).
        #[arg(long)]
        interval: Option<u64>,
        /// External IP-camera snapshot URL(s) the dashboard proxies (single JPEG
        /// per GET, e.g. an ATOM Cam `http://HOST/cgi-bin/get_jpeg.cgi`). Repeat the
        /// flag for multiple cameras, optionally labelling each as `label=url`. The
        /// dashboard shows them as tabs and can add/remove more at runtime. May also
        /// be set via $BAMBU_CAMERA_URL (comma-separated).
        #[arg(long, env = "BAMBU_CAMERA_URL", value_delimiter = ',')]
        camera_url: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Add or update a profile (named by --printer).
    Add {
        #[arg(long)]
        ip: String,
        #[arg(long)]
        serial: String,
        #[arg(long)]
        access_code: String,
        #[arg(long)]
        model: String,
        /// Make this the default profile.
        #[arg(long)]
        set_default: bool,
    },
    /// List saved profiles.
    List,
    /// Show a profile (access code redacted).
    Show,
}

#[derive(Subcommand)]
enum JobAction {
    /// Start a print of a file already on the printer (.gcode or .gcode.3mf).
    Start {
        /// On-printer path, e.g. /cache/foo.gcode or /cache/foo.gcode.3mf.
        file: String,
        /// Plate number (for .3mf project files).
        #[arg(long, default_value_t = 1)]
        plate: u32,
        /// Use the AMS with this mapping: comma-separated tray indices per
        /// filament, -1 = external spool (e.g. "0,-1").
        #[arg(long)]
        ams_map: Option<String>,
        /// Build-plate type.
        #[arg(long, default_value = "auto")]
        bed_type: String,
        /// Record a printer-side timelapse for this print (sets the
        /// project_file `timelapse` flag). Needs a working built-in camera.
        #[arg(long)]
        timelapse: bool,
        /// Show the resolved command JSON without sending it (safe).
        #[arg(long)]
        dry_run: bool,
        /// Required to actually start a print.
        #[arg(long)]
        confirm: bool,
        /// Guard: refuse unless the on-printer file's plate-gcode md5 matches
        /// this (case-insensitive). Get it from `--dry-run`. (.3mf only.)
        #[arg(long)]
        expect_md5: Option<String>,
        /// Guard: refuse unless --plate equals this. (.3mf only.)
        #[arg(long)]
        expect_plate: Option<u32>,
        /// After starting, watch the job to completion and detect anomalies
        /// (a device error or a FAILED state exits non-zero).
        #[arg(long)]
        watch: bool,
        /// With --watch, give up watching after this many seconds (default 6h).
        #[arg(long, default_value_t = 21600)]
        watch_timeout: u64,
        /// With --watch, poll every N seconds (sends `pushall`) for a higher
        /// data rate, like Bambu Studio. Default: passive.
        #[arg(long)]
        interval: Option<u64>,
    },
    /// Pause the current print (needs --confirm).
    Pause {
        #[arg(long)]
        confirm: bool,
    },
    /// Resume a paused print (needs --confirm).
    Resume {
        #[arg(long)]
        confirm: bool,
    },
    /// Stop (cancel) the current print — irreversible (needs --confirm).
    Stop {
        #[arg(long)]
        confirm: bool,
    },
}

/// Which calibration routine to run. Each is a separate subcommand so the choice
/// is explicit (e.g. `bambu calibrate bed-level`).
#[derive(Subcommand)]
enum CalibrateAction {
    /// Auto-level the heated bed.
    BedLevel(CalibrateArgs),
    /// Vibration / resonance compensation.
    Vibration(CalibrateArgs),
    /// Motor-noise (current) calibration.
    MotorNoise(CalibrateArgs),
    /// The usual A1 set: bed level + vibration.
    Auto(CalibrateArgs),
}

/// Flags shared by every `calibrate` subcommand.
#[derive(clap::Args)]
struct CalibrateArgs {
    /// Show what would run, without sending it (safe).
    #[arg(long)]
    dry_run: bool,
    /// Required to actually run calibration (it moves the hardware).
    #[arg(long)]
    confirm: bool,
    /// After starting, watch the printer report until calibration finishes.
    #[arg(long)]
    watch: bool,
    /// With --watch, give up watching after this many seconds (default 1h).
    #[arg(long, default_value_t = 3600)]
    watch_timeout: u64,
    /// With --watch, poll every N seconds (sends `pushall`) for a higher data
    /// rate. Default: passive (wait for the printer's own pushes).
    #[arg(long)]
    interval: Option<u64>,
}

#[derive(Subcommand)]
enum TimelapseAction {
    /// Enable printer-side timelapse recording (camera.ipcam_timelapse).
    Enable {
        /// Watch the report for this many seconds to confirm the setting.
        #[arg(long, default_value_t = 8)]
        timeout: u64,
    },
    /// Disable printer-side timelapse recording (camera.ipcam_timelapse).
    Disable {
        #[arg(long, default_value_t = 8)]
        timeout: u64,
    },
    /// List recorded timelapse files on the printer (FTPS /timelapse).
    List,
    /// Download a recorded timelapse file from the printer.
    Get {
        /// File name under /timelapse (or a full on-printer path).
        name: String,
        /// Local output path (default: the file's basename in the CWD).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Drive an EXTERNAL camera: watch the active print and run a capture
    /// command on each new layer (works even with no/!broken built-in camera).
    ///
    /// The capture command goes after `--` and runs as argv (no shell), so its
    /// own flags are fine. Tokens {frame} (the numbered output path), {layer} and
    /// {outdir} are substituted. E.g. an ATOM Cam / IP camera:
    ///   bambu timelapse capture --out-dir ./tl -- \
    ///     curl -s -m 15 -o {frame} http://$ATOMCAM_HOST/cgi-bin/get_jpeg.cgi
    Capture {
        /// Directory for captured frames (created if missing).
        #[arg(long, default_value = "./timelapse")]
        out_dir: std::path::PathBuf,
        /// Capture every Nth layer (1 = every layer).
        #[arg(long, default_value_t = 1)]
        every: u64,
        /// Frame file extension used for {frame} paths.
        #[arg(long, default_value = "jpg")]
        ext: String,
        /// Poll the printer every N seconds (sends `pushall`) for a higher layer
        /// detection rate. Default: passive (printer's ~2s push).
        #[arg(long)]
        interval: Option<u64>,
        /// Give up watching after this many seconds (default 6h).
        #[arg(long, default_value_t = 21600)]
        timeout: u64,
        /// Wait for a print to start instead of requiring one already running:
        /// sit through idle/finished states (and a stale error from the last
        /// print) and begin capturing once the print becomes active. Lets you
        /// launch this BEFORE starting the print. Bounded by --timeout.
        #[arg(long)]
        wait: bool,
        /// The capture command (after `--`), as argv: program then args, with
        /// {frame}/{layer}/{outdir} tokens. Run directly, never via a shell.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1.., value_name = "CMD")]
        on_layer_cmd: Vec<String>,
    },
}

#[derive(Subcommand)]
enum AmsAction {
    /// Resume the AMS after a pause/error (ams_control resume).
    Resume {
        #[arg(long)]
        confirm: bool,
    },
    /// Reset the AMS state (ams_control reset).
    Reset {
        #[arg(long)]
        confirm: bool,
    },
    /// Pause the AMS (ams_control pause).
    Pause {
        #[arg(long)]
        confirm: bool,
    },
    /// Change the loaded filament via the AMS — physically moves filament.
    Change {
        /// Target tray id.
        #[arg(long)]
        tray: u32,
        /// New nozzle temperature (°C) for the target filament.
        #[arg(long)]
        tar_temp: i64,
        /// Current nozzle temperature (°C); defaults to the new temp.
        #[arg(long)]
        curr_temp: Option<i64>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        confirm: bool,
    },
    /// Set a tray's filament profile (material/colour/temps).
    SetFilament {
        #[arg(long, default_value_t = 0)]
        ams: u32,
        #[arg(long)]
        tray: u32,
        /// Material, e.g. PLA, PETG.
        #[arg(long = "type")]
        material: String,
        /// Colour as hex RRGGBBAA (alpha usually FF).
        #[arg(long, default_value = "000000FF")]
        color: String,
        /// Min/max nozzle temperature (°C).
        #[arg(long)]
        min: i64,
        #[arg(long)]
        max: i64,
        /// Filament profile id (e.g. GFA00); optional.
        #[arg(long, default_value = "")]
        info_idx: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        confirm: bool,
    },
    /// Set AMS RFID-read options (ams_user_setting).
    Settings {
        #[arg(long, default_value_t = 0)]
        ams: u32,
        /// Read RFID on startup.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        startup_read: bool,
        /// Read RFID on tray insertion.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        tray_read: bool,
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
enum CameraAction {
    /// Grab one JPEG frame and write it to a file.
    Snapshot {
        /// Output file path.
        #[arg(long, default_value = "snapshot.jpg")]
        out: std::path::PathBuf,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 10)]
        timeout: u64,
    },
}

#[derive(Subcommand)]
enum FileAction {
    /// List file names in a directory on the printer.
    Ls {
        #[arg(default_value = "/")]
        dir: String,
    },
    /// Upload a local file to the printer.
    Upload {
        /// Local file to upload.
        local: std::path::PathBuf,
        /// Destination directory on the printer.
        #[arg(long, default_value = "/cache")]
        dest: String,
    },
    /// Download a file from the printer (e.g. a timelapse video).
    Download {
        /// On-printer path, e.g. /timelapse/video.mp4.
        remote: String,
        /// Local output path (default: the remote file's basename in the CWD).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Delete a file on the printer — irreversible (needs --confirm).
    Rm {
        /// On-printer path to delete.
        remote: String,
        #[arg(long)]
        confirm: bool,
    },
}

/// A CLI error carrying the exit code to return.
#[derive(Debug)]
struct CliError {
    code: u8,
    message: String,
}

impl CliError {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<ConfigError> for CliError {
    fn from(e: ConfigError) -> Self {
        let code = match e {
            ConfigError::MissingField(_) | ConfigError::UnknownProfile(_) => exit::VALIDATION,
            _ => exit::GENERAL,
        };
        CliError::new(code, e.to_string())
    }
}

impl From<ClientError> for CliError {
    fn from(e: ClientError) -> Self {
        let code = match e {
            ClientError::Timeout(_) => exit::VERIFY_TIMEOUT,
            _ => exit::TRANSPORT,
        };
        CliError::new(code, e.to_string())
    }
}

impl From<FtpError> for CliError {
    fn from(e: FtpError) -> Self {
        CliError::new(exit::TRANSPORT, e.to_string())
    }
}

impl From<CameraError> for CliError {
    fn from(e: CameraError) -> Self {
        CliError::new(exit::TRANSPORT, e.to_string())
    }
}

/// Entry point. Parses args, dispatches, and maps errors to exit codes.
pub fn run() -> ExitCode {
    // Pull BAMBU_* from a local .env (without overriding real env vars) so an
    // interactive user need not export them every time.
    config::load_dotenv();
    // With the `license-notice` feature (release builds), add a `--license-notice`
    // flag that prints the embedded third-party notices; otherwise a plain parse.
    #[cfg(feature = "license-notice")]
    let cli = {
        use notalawyer_clap::{ParseExt, include_notice};
        Cli::parse_with_license_notice(include_notice!())
    };
    #[cfg(not(feature = "license-notice"))]
    let cli = Cli::parse();
    match dispatch(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {}", e.message);
            ExitCode::from(e.code)
        }
    }
}

fn dispatch(cli: &Cli) -> Result<(), CliError> {
    match &cli.command {
        Command::Config { action } => run_config(cli, action),
        Command::Status {
            watch,
            interval,
            timeout,
        } => run_status(cli, *watch, *interval, *timeout),
        Command::Info => run_info(cli),
        Command::Hms => run_hms(cli),
        Command::Job { action } => run_job(cli, action),
        Command::File { action } => run_file(cli, action),
        Command::Camera { action } => run_camera(cli, action),
        Command::Timelapse { action } => run_timelapse(cli, action),
        Command::Ams { action } => run_ams(cli, action),
        Command::Light {
            state,
            node,
            timeout,
        } => run_light(cli, state == "on", node, *timeout),
        Command::Speed { level, timeout } => run_speed(cli, level, *timeout),
        Command::Calibrate { what } => run_calibrate(cli, what),
        Command::Gcode {
            line,
            confirm,
            force,
            timeout,
        } => run_gcode(cli, line, *confirm, *force, *timeout),
        Command::Reboot { confirm } => run_reboot(cli, *confirm),
        #[cfg(feature = "server")]
        Command::Serve {
            host,
            port,
            password,
            fake,
            interval,
            camera_url,
        } => run_serve(
            cli,
            host,
            *port,
            password.clone(),
            *fake,
            *interval,
            camera_url.clone(),
        ),
    }
}

fn config_path() -> Result<std::path::PathBuf, CliError> {
    config::default_config_path()
        .ok_or_else(|| CliError::new(exit::GENERAL, "cannot determine config path (no HOME)"))
}

fn run_config(cli: &Cli, action: &ConfigAction) -> Result<(), CliError> {
    let path = config_path()?;
    let mut cfg = Config::load_or_default(&path)?;
    match action {
        ConfigAction::Add {
            ip,
            serial,
            access_code,
            model,
            set_default,
        } => {
            let name = cli.printer.clone().ok_or_else(|| {
                CliError::new(exit::VALIDATION, "config add needs --printer <name>")
            })?;
            let profile = Profile {
                ip: ip.clone(),
                serial: serial.clone(),
                model: model.clone(),
                mode: "lan".to_string(),
                access_code: access_code.clone(),
            };
            cfg.printers.insert(name.clone(), profile);
            if *set_default || cfg.default_printer.is_none() {
                cfg.default_printer = Some(name.clone());
            }
            cfg.save(&path)?;
            eprintln!("saved profile '{name}' to {}", path.display());
            Ok(())
        }
        ConfigAction::List => {
            if want_json(cli) {
                let names: Vec<&String> = cfg.printers.keys().collect();
                print_json(&serde_json::json!({
                    "default": cfg.default_printer,
                    "printers": names,
                }));
            } else if cfg.printers.is_empty() {
                eprintln!("no profiles configured");
            } else {
                for name in cfg.printers.keys() {
                    let marker = if cfg.default_printer.as_deref() == Some(name) {
                        " (default)"
                    } else {
                        ""
                    };
                    println!("{name}{marker}");
                }
            }
            Ok(())
        }
        ConfigAction::Show => {
            let name = selected_profile_name(cli, &cfg)?.ok_or_else(|| {
                CliError::new(
                    exit::VALIDATION,
                    "no printer selected: pass --printer or set a default",
                )
            })?;
            let profile = cfg
                .profile(&name)
                .ok_or_else(|| CliError::from(ConfigError::UnknownProfile(name.clone())))?;
            let view = RedactedProfile::from(&name, profile);
            if want_json(cli) {
                print_json(&view);
            } else {
                println!("{view}");
            }
            Ok(())
        }
    }
}

fn run_status(
    cli: &Cli,
    watch: bool,
    interval_secs: Option<u64>,
    timeout_secs: u64,
) -> Result<(), CliError> {
    let cfg = Config::load_or_default(&config_path()?)?;
    let profile_name = selected_profile_name(cli, &cfg)?;
    let profile = profile_name.as_deref().and_then(|n| cfg.profile(n));
    let overrides = flag_overrides(cli).over(Overrides::from_env());
    let target = config::resolve(profile, &overrides)?;
    let model = target.model.to_string();

    if watch {
        // Continuous monitor: live updates that do NOT stop at job completion
        // (runs until --timeout or Ctrl-C). Output goes to stdout.
        let client = LanMqttClient::new(target).with_timeout(Duration::from_secs(timeout_secs));
        let interval = interval_secs.map(Duration::from_secs);
        return watch_to_terminal(&client, cli, model, profile_name, false, interval, true);
    }

    let state = LanMqttClient::new(target).fetch_snapshot()?;
    let status = PrinterStatus::from_state(state.get());
    let output = StatusOutput {
        printer: profile_name,
        model,
        status,
    };
    if want_json(cli) {
        print_json(&output);
    } else {
        print_status_human(&output);
    }
    Ok(())
}

/// One decoded HMS alert, for output.
#[derive(Serialize)]
struct HmsView {
    code: String,
    code_hyphen: String,
    severity: u16,
    is_lidar: bool,
    wiki: String,
}

fn run_hms(cli: &Cli) -> Result<(), CliError> {
    let state = connect_client(cli, 10)?.fetch_snapshot()?;
    let entries = crate::core::hms::decode_report_hms(state.get());
    let views: Vec<HmsView> = entries
        .iter()
        .map(|e| HmsView {
            code: e.code_string(),
            code_hyphen: e.code_hyphen(),
            severity: e.severity_raw(),
            is_lidar: e.is_lidar(),
            wiki: e.wiki_url(),
        })
        .collect();

    if want_json(cli) {
        print_json(&views);
    } else if views.is_empty() {
        println!("no active HMS alerts");
    } else {
        for v in &views {
            println!("{}  (severity {})  {}", v.code, v.severity, v.wiki);
        }
    }
    Ok(())
}

/// Agent-facing view of the control assessment (degrade-not-wall).
#[derive(Serialize)]
struct ControlView {
    /// `allowed` | `requires_developer_mode` | `newer_firmware_untested` | `refused`.
    status: &'static str,
    /// Whether control is *expected* to work (true for the first three).
    expected_ok: bool,
    /// Human-readable reason, present for warnings/refusals.
    reason: Option<String>,
}

impl ControlView {
    fn from(assessment: ControlAssessment) -> Self {
        let refusal = |r: ControlRefusal| match r {
            ControlRefusal::UnknownModel => "model not in the capability registry",
            ControlRefusal::FirmwareNewerThanKnown => "firmware newer than the registry knows",
            ControlRefusal::DeveloperModeUnavailable => {
                "Developer Mode unavailable on this firmware"
            }
            ControlRefusal::UnknownControlBoundary => {
                "no confirmed control boundary for this model"
            }
        };
        match assessment {
            ControlAssessment::Allowed => ControlView {
                status: "allowed",
                expected_ok: true,
                reason: None,
            },
            ControlAssessment::RequiresDeveloperMode => ControlView {
                status: "requires_developer_mode",
                expected_ok: true,
                reason: Some("control needs LAN-only + Developer Mode enabled".into()),
            },
            ControlAssessment::NewerFirmwareUntested => ControlView {
                status: "newer_firmware_untested",
                expected_ok: true,
                reason: Some(
                    "firmware is newer than the tested range; control is very likely fine but \
                     unverified against this version"
                        .into(),
                ),
            },
            ControlAssessment::Refused(r) => ControlView {
                status: "refused",
                expected_ok: false,
                reason: Some(refusal(r).into()),
            },
        }
    }
}

/// Output of `bambu info`: identity + firmware + resolved capabilities.
#[derive(Serialize)]
struct InfoOutput {
    printer: Option<String>,
    model: String,
    firmware: Option<String>,
    registry_status: &'static str,
    push_mode: Option<&'static str>,
    camera_transport: Option<&'static str>,
    developer_mode: Option<&'static str>,
    control: ControlView,
    modules: Vec<Module>,
}

fn run_info(cli: &Cli) -> Result<(), CliError> {
    let cfg = Config::load_or_default(&config_path()?)?;
    let profile_name = selected_profile_name(cli, &cfg)?;
    let profile = profile_name.as_deref().and_then(|n| cfg.profile(n));
    let overrides = flag_overrides(cli).over(Overrides::from_env());
    let target = config::resolve(profile, &overrides)?;
    let model = target.model.clone();

    let version = connect_client(cli, 10)?.fetch_version()?;

    // Resolve capabilities only when the firmware is known; without it we can
    // still report descriptive facts via a model-only lookup is not possible
    // (resolve needs a firmware), so we fall back to reporting "unknown firmware".
    let registry = capability::default_registry();
    let output = match &version.firmware {
        Some(fw) => {
            let caps = capability::resolve(&registry, &model, fw);
            InfoOutput {
                printer: profile_name,
                model: model.to_string(),
                firmware: Some(fw.to_string()),
                registry_status: registry_status_str(caps.registry_status),
                push_mode: caps.push_mode.map(push_mode_str),
                camera_transport: caps.camera_transport.map(camera_transport_str),
                developer_mode: caps.developer_mode.map(developer_mode_str),
                control: ControlView::from(caps.control_assessment()),
                modules: version.modules.clone(),
            }
        }
        None => InfoOutput {
            printer: profile_name,
            model: model.to_string(),
            firmware: None,
            registry_status: "unknown_firmware",
            push_mode: None,
            camera_transport: None,
            developer_mode: None,
            control: ControlView {
                status: "unknown",
                expected_ok: false,
                reason: Some("could not read the firmware version (no `ota` module)".into()),
            },
            modules: version.modules.clone(),
        },
    };

    if want_json(cli) {
        print_json(&output);
    } else {
        print_info_human(&output);
    }
    Ok(())
}

fn registry_status_str(s: capability::RegistryStatus) -> &'static str {
    use capability::RegistryStatus::*;
    match s {
        Supported => "supported",
        FirmwareNewerThanKnown => "firmware_newer_than_known",
        UnknownModel => "unknown_model",
    }
}

fn push_mode_str(m: capability::PushMode) -> &'static str {
    match m {
        capability::PushMode::Full => "full",
        capability::PushMode::DeltaOnly => "delta_only",
    }
}

fn camera_transport_str(t: capability::CameraTransport) -> &'static str {
    use capability::CameraTransport::*;
    match t {
        Rtsp322 => "rtsp_322",
        JpegTcp6000 => "jpeg_tcp_6000",
        None => "none",
    }
}

fn developer_mode_str(d: capability::DeveloperMode) -> &'static str {
    match d {
        capability::DeveloperMode::Available => "available",
        capability::DeveloperMode::Unavailable => "unavailable",
    }
}

fn print_info_human(o: &InfoOutput) {
    println!(
        "printer: {} ({})",
        o.printer.as_deref().unwrap_or("-"),
        o.model
    );
    println!("firmware: {}", o.firmware.as_deref().unwrap_or("?"));
    println!("registry: {}", o.registry_status);
    if let Some(p) = o.push_mode {
        println!("push:     {p}");
    }
    if let Some(c) = o.camera_transport {
        println!("camera:   {c}");
    }
    match &o.control.reason {
        Some(r) => println!("control:  {} — {r}", o.control.status),
        None => println!("control:  {}", o.control.status),
    }
    if !o.modules.is_empty() {
        println!("modules:");
        for m in &o.modules {
            let hw = m.hw_ver.as_deref().unwrap_or("-");
            let sw = m.sw_ver.as_deref().unwrap_or("-");
            let prod = m
                .product_name
                .as_deref()
                .map(|p| format!("  {p}"))
                .unwrap_or_default();
            println!("  {:<10} hw {:<9} sw {}{prod}", m.name, hw, sw);
        }
    }
}

/// The report fields whose change triggers a new `watch` progress line.
/// Temperatures are rounded to whole °C so heating/cooling is visible (each 1 °C
/// step prints a line) without spamming on sub-degree jitter.
#[derive(PartialEq)]
struct WatchKey {
    gcode_state: Option<String>,
    stg_cur: Option<i64>,
    mc_percent: Option<i64>,
    layer_num: Option<i64>,
    nozzle: Option<i64>,
    bed: Option<i64>,
    error: Option<i64>,
}

/// Watch the printer to a terminal state, **or until a device error appears**,
/// printing a progress line (to stderr) on every change. Used by `watch` and by
/// `job start --watch`. A `print_error` mid-job is treated as an anomaly: stop,
/// surface it, and exit non-zero regardless of `exit_status`. `exit_status`
/// additionally makes a FAILED end-state exit non-zero (gh-run-watch style).
fn watch_to_terminal(
    client: &LanMqttClient,
    cli: &Cli,
    model: String,
    profile_name: Option<String>,
    exit_status: bool,
    interval: Option<Duration>,
    continuous: bool,
) -> Result<(), CliError> {
    let mut last: Option<WatchKey> = None;
    let mut on_update = |state: &ReportState| -> WatchStep {
        let st = PrinterStatus::from_state(state.get());
        let key = WatchKey {
            gcode_state: st.gcode_state.clone(),
            stg_cur: st.stg_cur,
            mc_percent: st.mc_percent,
            layer_num: st.layer_num,
            nozzle: st.nozzle_temper.map(|v| v.round() as i64),
            bed: st.bed_temper.map(|v| v.round() as i64),
            error: st.error.as_ref().map(|e| e.code),
        };
        if last.as_ref() != Some(&key) {
            last = Some(key);
            let stage = match (st.stg_cur, st.stage) {
                (Some(id), Some(name)) if !Stage(id).is_no_stage() => format!("  [{name}]"),
                _ => String::new(),
            };
            let err = match &st.error {
                Some(e) => format!("  ⚠ {}", e.hex),
                None => String::new(),
            };
            // Nozzle/bed as current°→target° (target omitted when off/unset).
            let temp = |cur: Option<f64>, tgt: Option<f64>| match cur {
                Some(c) => match tgt.filter(|t| *t > 0.0) {
                    Some(t) => format!("{c:.0}/{t:.0}"),
                    None => format!("{c:.0}"),
                },
                None => "-".to_string(),
            };
            let eta = match st.remaining_time_min.filter(|m| *m > 0) {
                Some(m) => format!("  ETA {}", fmt_eta(m)),
                None => String::new(),
            };
            let line = format!(
                "{:<8} {:>3}%  layer {}/{}  N{} B{}{eta}{stage}{err}",
                st.gcode_state.as_deref().unwrap_or("?"),
                st.mc_percent.unwrap_or(0),
                st.layer_num.unwrap_or(0),
                st.total_layer_num.unwrap_or(0),
                temp(st.nozzle_temper, st.nozzle_target),
                temp(st.bed_temper, st.bed_target),
            );
            // Continuous monitor (`status --watch`) is the command's own output →
            // stdout (NDJSON under --json). A to-terminal watch keeps stdout clean
            // for the final snapshot, so its progress goes to stderr.
            if continuous {
                if want_json(cli) {
                    if let Ok(j) = serde_json::to_string(&st) {
                        println!("{j}");
                    }
                } else {
                    println!("{line}");
                }
            } else {
                eprintln!("{line}");
            }
        }
        // A continuous monitor never stops on its own (runs until timeout / Ctrl-C).
        if continuous {
            return WatchStep::Continue;
        }
        // A device fault is an anomaly worth stopping for, even mid-RUNNING.
        if st.error.is_some() {
            return WatchStep::Stop;
        }
        match st.state() {
            Some(s) if is_watch_terminal(s) => WatchStep::Stop,
            _ => WatchStep::Continue,
        }
    };

    // status --watch monitors (reconnects, stall timeout); job start --watch
    // watches a job to completion (fail-fast).
    let result = if continuous {
        client.monitor(interval, &mut on_update)
    } else {
        client.watch(interval, &mut on_update)
    };
    let final_state = result?;
    // The monitor's per-change lines were the output; it ends only via its
    // stall window (or Ctrl-C) — nothing more to print, no exit codes.
    if continuous {
        return Ok(());
    }

    let status = PrinterStatus::from_state(final_state.get());
    let error = status.error.clone();
    let failed = status.state() == Some(GcodeState::Failed);
    let output = StatusOutput {
        printer: profile_name,
        model,
        status,
    };
    if want_json(cli) {
        print_json(&output);
    } else {
        print_status_human(&output);
    }
    if let Some(e) = error {
        return Err(CliError::new(
            exit::DEVICE_REJECTED,
            format!(
                "a device error appeared during the job: {} ({})",
                e.hex, e.code
            ),
        ));
    }
    if exit_status && failed {
        return Err(CliError::new(
            exit::GENERAL,
            "print ended in a FAILED state",
        ));
    }
    Ok(())
}

/// Resolve `(model string, profile name)` for status/watch output headers,
/// using the same precedence as a connection.
fn watch_identity(cli: &Cli) -> Result<(String, Option<String>), CliError> {
    let cfg = Config::load_or_default(&config_path()?)?;
    let profile_name = selected_profile_name(cli, &cfg)?;
    let profile = profile_name.as_deref().and_then(|n| cfg.profile(n));
    let overrides = flag_overrides(cli).over(Overrides::from_env());
    let target = config::resolve(profile, &overrides)?;
    Ok((target.model.to_string(), profile_name))
}

fn run_light(cli: &Cli, on: bool, node: &str, timeout_secs: u64) -> Result<(), CliError> {
    let node = match node {
        "chamber" => LedNode::ChamberLight,
        "work" => LedNode::WorkLight,
        other => {
            return Err(CliError::new(
                exit::VALIDATION,
                format!("unknown light {other:?}"),
            ));
        }
    };
    let client = connect_client(cli, timeout_secs)?;
    eprintln!(
        "setting {} {} …",
        node.as_str(),
        if on { "on" } else { "off" }
    );
    report_command_outcome(
        cli,
        client.send_and_verify(&ProtoCommand::Led { node, on })?,
    )
}

fn run_speed(cli: &Cli, level: &str, timeout_secs: u64) -> Result<(), CliError> {
    let level = match level {
        "silent" => SpeedLevel::Silent,
        "standard" => SpeedLevel::Standard,
        "sport" => SpeedLevel::Sport,
        "ludicrous" => SpeedLevel::Ludicrous,
        other => {
            return Err(CliError::new(
                exit::VALIDATION,
                format!("unknown speed {other:?}"),
            ));
        }
    };
    let client = connect_client(cli, timeout_secs)?;
    eprintln!(
        "setting print speed to {} (level {}) …",
        level.as_str(),
        level.level()
    );
    report_command_outcome(
        cli,
        client.send_and_verify(&ProtoCommand::PrintSpeed(level))?,
    )
}

fn run_reboot(cli: &Cli, confirm: bool) -> Result<(), CliError> {
    if !confirm {
        return Err(CliError::new(
            exit::CONFIRM_REQUIRED,
            "refusing to reboot without --confirm (the printer will disconnect and restart)",
        ));
    }
    let client = connect_client(cli, 10)?;
    eprintln!("sending reboot …");
    // Reboot tears down the connection, so there is no ACK — fire-and-forget.
    client.send_fire(&ProtoCommand::Reboot)?;
    eprintln!(
        "reboot sent — the printer will disconnect and restart (~1–2 min). \
         No ACK is expected; it may rejoin DHCP on a different IP."
    );
    Ok(())
}

#[cfg(feature = "server")]
#[allow(clippy::too_many_arguments)]
fn run_serve(
    cli: &Cli,
    host: &str,
    port: u16,
    password: Option<String>,
    fake: bool,
    interval: Option<u64>,
    camera_url: Vec<String>,
) -> Result<(), CliError> {
    // Live mode needs a connection target; fake mode doesn't touch the printer.
    let target = if fake {
        None
    } else {
        Some(resolve_target(cli)?)
    };
    // Parse each `--camera-url` entry (`label=url` or a bare `url`) into a labelled
    // external camera, dropping blanks; the post-filter index gives stable
    // sequential auto-labels (external 1, external 2, …).
    let external_cameras = camera_url
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .enumerate()
        .filter_map(|(i, e)| crate::server::ExternalCamera::parse(e, i))
        .collect();
    let opts = crate::server::ServeOpts {
        host: host.to_string(),
        port,
        password,
        fake,
        interval: interval.map(Duration::from_secs),
        external_cameras,
    };
    crate::server::serve(target, opts).map_err(|e| CliError::new(exit::GENERAL, e.to_string()))
}

fn run_gcode(
    cli: &Cli,
    line: &str,
    confirm: bool,
    force: bool,
    timeout_secs: u64,
) -> Result<(), CliError> {
    if !confirm {
        return Err(CliError::new(
            exit::CONFIRM_REQUIRED,
            "refusing to send a control command without --confirm",
        ));
    }
    // Static safety guard: block recognised-dangerous lines (over-limit temps,
    // cold extrusion) unless explicitly overridden with --force.
    if !force && let GcodeVerdict::Block(reason) = safety::check_gcode(line, &TempLimits::default())
    {
        return Err(CliError::new(
            exit::VALIDATION,
            format!("refusing unsafe G-code: {reason}"),
        ));
    }
    let client = connect_client(cli, timeout_secs)?;
    eprintln!("sending gcode_line {line:?} …");
    report_command_outcome(
        cli,
        client.send_and_verify(&ProtoCommand::GcodeLine(line.to_string()))?,
    )
}

fn run_file(cli: &Cli, action: &FileAction) -> Result<(), CliError> {
    let ftps = FtpsClient::new(resolve_target(cli)?);
    match action {
        FileAction::Ls { dir } => {
            let names = ftps.list(dir)?;
            if want_json(cli) {
                print_json(&names);
            } else {
                for name in &names {
                    println!("{name}");
                }
            }
            Ok(())
        }
        FileAction::Upload { local, dest } => {
            let filename = local
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| CliError::new(exit::VALIDATION, "invalid local file name"))?;
            let remote = format!("{}/{filename}", dest.trim_end_matches('/'));
            let n = ftps.upload(local, &remote)?;
            eprintln!("uploaded {n} bytes to {remote}");
            Ok(())
        }
        FileAction::Download { remote, out } => {
            let local = match out {
                Some(p) => p.clone(),
                None => std::path::Path::new(remote)
                    .file_name()
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| {
                        CliError::new(
                            exit::VALIDATION,
                            format!("cannot derive an output name from {remote:?}; pass --out"),
                        )
                    })?,
            };
            let n = ftps.download(remote, &local)?;
            eprintln!("downloaded {n} bytes to {}", local.display());
            if want_json(cli) {
                print_json(&serde_json::json!({
                    "path": local.to_string_lossy(),
                    "bytes": n,
                }));
            } else {
                // The file path is the result (never the file's bytes).
                println!("{}", local.display());
            }
            Ok(())
        }
        FileAction::Rm { remote, confirm } => {
            if !*confirm {
                return Err(CliError::new(
                    exit::CONFIRM_REQUIRED,
                    "refusing to delete a file without --confirm",
                ));
            }
            ftps.delete(remote)?;
            eprintln!("deleted {remote}");
            if want_json(cli) {
                print_json(&serde_json::json!({ "deleted": true, "remote": remote }));
            }
            Ok(())
        }
    }
}

fn run_job(cli: &Cli, action: &JobAction) -> Result<(), CliError> {
    match action {
        JobAction::Start {
            file,
            plate,
            ams_map,
            bed_type,
            timelapse,
            dry_run,
            confirm,
            expect_md5,
            expect_plate,
            watch,
            watch_timeout,
            interval,
        } => {
            let is_3mf = file.to_ascii_lowercase().ends_with(".3mf");
            // The expect-guards are 3mf-only (raw .gcode has no plate/md5 metadata):
            // reject them on a .gcode rather than silently ignore.
            if !is_3mf && (expect_md5.is_some() || expect_plate.is_some()) {
                return Err(CliError::new(
                    exit::VALIDATION,
                    "--expect-md5 / --expect-plate only apply to .3mf files",
                ));
            }
            let cmd = build_start_command(file, *plate, ams_map.as_deref(), bed_type, *timelapse)?;

            // The AMS mapping (if any), for validation + dry-run preview.
            let ams_mapping: Option<Vec<i32>> = match &cmd {
                ProtoCommand::ProjectFile(pf) if pf.use_ams => Some(pf.ams_mapping.clone()),
                _ => None,
            };
            // Tray-range is cheap and needs no inspection — fail fast on EVERY
            // path (even a plain confirm or an unreachable printer). The
            // filament-count match needs the 3mf, so it runs below once inspected.
            if let Some(m) = &ams_mapping {
                validate_ams_map(m, None)?;
            }

            // When an expect-guard is given, inspecting the on-printer file is
            // MANDATORY (the caller asked us to verify) and a mismatch is fatal.
            // A bare --dry-run inspects BEST-EFFORT (enrich the plan if the
            // printer is reachable, else show the payload alone). A plain start
            // doesn't inspect at all — that path stays fast and unchanged.
            let has_expect = expect_md5.is_some() || expect_plate.is_some();
            let mut inspection: Option<PlateInspection> = None;
            // For a best-effort dry-run, remember why inspection failed so the
            // plan can say so explicitly (never a silent/ambiguous null).
            let mut inspect_error: Option<String> = None;
            if is_3mf && (has_expect || ams_mapping.is_some() || *dry_run) {
                let mandatory = has_expect || ams_mapping.is_some();
                match inspect_remote_plate(cli, file, *plate) {
                    Ok(insp) => {
                        project::verify_expectations(
                            &insp,
                            *plate,
                            expect_md5.as_deref(),
                            *expect_plate,
                        )
                        .map_err(|e| CliError::new(exit::VALIDATION, e.to_string()))?;
                        // Filament-count check now that we know the plate's
                        // filaments. On a real start a mismatch is fatal (exit 3);
                        // on --dry-run it's downgraded to a warning so the plan
                        // (which also flags it) still prints for the agent to fix.
                        if let Some(m) = &ams_mapping {
                            match validate_ams_map(m, Some(insp.filament_colors.len())) {
                                Ok(warns) => {
                                    for w in warns {
                                        eprintln!("warning: {w}");
                                    }
                                }
                                Err(e) if *dry_run => eprintln!("warning: {}", e.message),
                                Err(e) => return Err(e),
                            }
                        }
                        inspection = Some(insp);
                    }
                    // Inspection is mandatory when an expect-guard or an AMS
                    // mapping needs the filament count; only best-effort for a
                    // bare dry-run.
                    Err(e) if mandatory => return Err(e),
                    Err(e) => {
                        eprintln!(
                            "note: could not inspect the on-printer file ({}); \
                             showing the payload only",
                            e.message
                        );
                        inspect_error = Some(e.message);
                    }
                }
            }

            if *dry_run {
                // Real plan: the resolved payload + what the on-printer file holds.
                print_json(&start_plan_json(
                    &cmd,
                    file,
                    inspection.as_ref(),
                    inspect_error.as_deref(),
                    ams_mapping.as_deref(),
                ));
                return Ok(());
            }
            if !*confirm {
                return Err(CliError::new(
                    exit::CONFIRM_REQUIRED,
                    "refusing to start a print without --confirm (try --dry-run first)",
                ));
            }
            ensure_idle(cli)?;
            let client = connect_client(cli, 30)?;
            eprintln!("starting print: {file}");
            let outcome = client.send_and_verify(&cmd)?;
            // Only keep watching if the print actually started; otherwise the
            // verdict (rejected/unverified) is the result.
            if *watch && outcome == CommandOutcome::Verified {
                eprintln!("print started; watching for completion / anomalies …");
                let (model, profile_name) = watch_identity(cli)?;
                let watcher = connect_client(cli, *watch_timeout)?;
                let watch_interval = interval.map(Duration::from_secs);
                watch_to_terminal(
                    &watcher,
                    cli,
                    model,
                    profile_name,
                    true,
                    watch_interval,
                    false,
                )
            } else {
                report_command_outcome(cli, outcome)
            }
        }
        JobAction::Pause { confirm } => job_control(cli, ProtoCommand::Pause, *confirm),
        JobAction::Resume { confirm } => job_control(cli, ProtoCommand::Resume, *confirm),
        JobAction::Stop { confirm } => job_control(cli, ProtoCommand::Stop, *confirm),
    }
}

/// Build the start command, choosing project_file (.3mf) or gcode_file (.gcode).
fn build_start_command(
    file: &str,
    plate: u32,
    ams_map: Option<&str>,
    bed_type: &str,
    timelapse: bool,
) -> Result<ProtoCommand, CliError> {
    let name = std::path::Path::new(file)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(file)
        .to_string();
    if file.to_ascii_lowercase().ends_with(".3mf") {
        // FTP-uploaded files live under a path like /cache/x.gcode.3mf -> ftp:///cache/...
        let mut pf = ProjectFile::new(format!("ftp://{file}"), plate, name);
        pf.bed_type = bed_type.to_string();
        pf.timelapse = timelapse;
        if let Some(map) = ams_map {
            pf.use_ams = true;
            pf.ams_mapping = parse_ams_map(map)?;
        }
        Ok(ProtoCommand::ProjectFile(pf))
    } else {
        Ok(ProtoCommand::GcodeFile(file.to_string()))
    }
}

fn parse_ams_map(map: &str) -> Result<Vec<i32>, CliError> {
    map.split(',')
        .map(|s| s.trim().parse::<i32>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CliError::new(exit::VALIDATION, format!("invalid --ams-map: {map:?}")))
}

/// Validate a parsed `--ams-map`. Tray range is **always** checked (needs only
/// the mapping); the filament-count match is checked only when `filament_count`
/// is known (we have to inspect the on-printer 3mf for that). A wrong mapping is
/// the AMS footgun the plan calls out — refuse (exit 3) rather than mis-print.
/// Returns warnings (non-fatal advisories) for the caller to surface.
fn validate_ams_map(
    mapping: &[i32],
    filament_count: Option<usize>,
) -> Result<Vec<String>, CliError> {
    // Range: A1 AMS Lite has trays 0..=3; -1 = external spool.
    for (i, &v) in mapping.iter().enumerate() {
        if !(-1..=3).contains(&v) {
            return Err(CliError::new(
                exit::VALIDATION,
                format!(
                    "--ams-map[{i}]={v} is out of range (AMS trays are 0..3, or -1 for the \
                     external spool)"
                ),
            ));
        }
    }
    if let Some(n) = filament_count
        && mapping.len() != n
    {
        return Err(CliError::new(
            exit::VALIDATION,
            format!(
                "--ams-map has {} entr{} but the plate has {n} filament(s) — one tray per \
                 filament, in order",
                mapping.len(),
                if mapping.len() == 1 { "y" } else { "ies" },
            ),
        ));
    }
    let mut warnings = Vec::new();
    if mapping.iter().filter(|&&v| v == -1).count() > 1 {
        warnings.push(
            "more than one filament is mapped to the external spool (-1); only one filament can \
             physically feed from it — verify this is intended"
                .to_string(),
        );
    }
    Ok(warnings)
}

/// Build the dry-run `ams_mapping_preview`: one entry per plate filament, pairing
/// its colour (the device-confirmed count source) with the tray it's mapped to.
fn ams_mapping_preview(colors: &[String], mapping: &[i32]) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = mapping
        .iter()
        .enumerate()
        .map(|(i, &tray)| {
            let source = if tray == -1 {
                "external spool".to_string()
            } else {
                format!("AMS tray {tray}")
            };
            serde_json::json!({
                "filament": i,
                "color": colors.get(i),
                "tray": tray,
                "source": source,
            })
        })
        .collect();
    serde_json::Value::Array(entries)
}

/// Download the on-printer `.3mf` to a temp file and inspect the given plate.
/// The temp file is always removed (success or error). A download failure maps
/// to exit 7 (transport), a parse/missing-plate to exit 3 (validation).
fn inspect_remote_plate(
    cli: &Cli,
    on_printer_path: &str,
    plate: u32,
) -> Result<PlateInspection, CliError> {
    let ftps = FtpsClient::new(resolve_target(cli)?);
    // Download into a freshly-created, randomly-named temp DIR (O_EXCL): an
    // attacker can't pre-create/symlink a path they can't predict, and the dir
    // (with the downloaded file and its `.part`) is RAII-removed on every exit
    // path — normal, `?`-error, or panic. Avoids the classic /tmp symlink/TOCTOU.
    let dir = tempfile::Builder::new()
        .prefix("bambu-inspect-")
        .tempdir()
        .map_err(|e| CliError::new(exit::GENERAL, format!("creating temp dir: {e}")))?;
    let tmp = dir.path().join("inspect.3mf");
    ftps.download(on_printer_path, &tmp)?; // FtpError -> exit 7
    let bytes = std::fs::read(&tmp)
        .map_err(|e| CliError::new(exit::GENERAL, format!("reading downloaded 3mf: {e}")))?;
    project::inspect_plate(&bytes, plate)
        .map_err(|e| CliError::new(exit::VALIDATION, format!("3mf inspection: {e}")))
    // `dir` drops here (or at any `?` above) -> the temp dir is removed.
}

/// Build the `--dry-run` plan: the exact command payload plus what the
/// on-printer file actually contains (so an agent can read the md5/plate and
/// pass them back as `--expect-md5`/`--expect-plate`).
fn start_plan_json(
    cmd: &ProtoCommand,
    file: &str,
    inspection: Option<&PlateInspection>,
    inspect_error: Option<&str>,
    ams_mapping: Option<&[i32]>,
) -> serde_json::Value {
    let inspection_json = match (inspection, inspect_error) {
        // Inspected the on-printer file successfully.
        (Some(i), _) => {
            let mut warnings: Vec<String> = Vec::new();
            if !i.sidecar_matches {
                warnings.push(
                    "the file's own .gcode.md5 sidecar disagrees with the computed md5; \
                     using the computed value"
                        .to_string(),
                );
            }
            // Pair each filament with the tray it'll draw from, so the mapping can
            // be eyeballed before --confirm (the plan's mandatory AMS preview).
            let ams_preview = ams_mapping.map(|m| ams_mapping_preview(&i.filament_colors, m));
            if let Some(m) = ams_mapping
                && m.len() != i.filament_colors.len()
            {
                warnings.push(format!(
                    "--ams-map has {} entries but the plate has {} filament(s)",
                    m.len(),
                    i.filament_colors.len()
                ));
            }
            serde_json::json!({
                "inspected": true,
                "file": file,
                "plate": i.plate,
                "gcode_md5": i.gcode_md5,
                "sidecar_md5": i.sidecar_md5,
                "sidecar_matches": i.sidecar_matches,
                "bed_type": i.bed_type,
                "filament_colors": i.filament_colors,
                "ams_mapping_preview": ams_preview,
                "source": "on-printer file (downloaded for inspection)",
                "warnings": warnings,
            })
        }
        // Best-effort inspection was attempted but failed — say so explicitly,
        // so an agent reading stdout never mistakes "couldn't check" for "fine".
        (None, Some(err)) => serde_json::json!({
            "inspected": false,
            "error": err,
        }),
        // No inspection applies (raw .gcode has no plate/md5 metadata).
        (None, None) => serde_json::Value::Null,
    };
    serde_json::json!({
        "command": cmd.to_payload("1"),
        "inspection": inspection_json,
    })
}

/// Refuse to start a print unless the printer is idle (a key safety guard).
fn ensure_idle(cli: &Cli) -> Result<(), CliError> {
    let state = connect_client(cli, 10)?.fetch_snapshot()?;
    match PrinterStatus::from_state(state.get()).state() {
        None | Some(GcodeState::Idle) | Some(GcodeState::Finish) | Some(GcodeState::Failed) => {
            Ok(())
        }
        Some(busy) => Err(CliError::new(
            exit::PRINTER_BUSY,
            format!("printer is busy ({busy:?}); refusing to start a print"),
        )),
    }
}

fn run_ams(cli: &Cli, action: &AmsAction) -> Result<(), CliError> {
    // Helper: a plain control command gated on --confirm (ACK-verified).
    let control =
        |cli: &Cli, cmd: ProtoCommand, confirm: bool, what: &str| -> Result<(), CliError> {
            if !confirm {
                return Err(CliError::new(
                    exit::CONFIRM_REQUIRED,
                    format!("{what} needs --confirm"),
                ));
            }
            let client = connect_client(cli, 15)?;
            eprintln!("{what} … (AMS commands are [spec]; the ACK confirms acceptance)");
            report_command_outcome(cli, client.send_and_verify(&cmd)?)
        };
    match action {
        AmsAction::Resume { confirm } => control(
            cli,
            ProtoCommand::AmsControl(AmsControl::Resume),
            *confirm,
            "ams resume",
        ),
        AmsAction::Reset { confirm } => control(
            cli,
            ProtoCommand::AmsControl(AmsControl::Reset),
            *confirm,
            "ams reset",
        ),
        AmsAction::Pause { confirm } => control(
            cli,
            ProtoCommand::AmsControl(AmsControl::Pause),
            *confirm,
            "ams pause",
        ),
        AmsAction::Change {
            tray,
            tar_temp,
            curr_temp,
            dry_run,
            confirm,
        } => {
            // Guard the nozzle temps the same way the raw-gcode guard does, so an
            // AMS change can't command an unsafe temperature.
            let max = TempLimits::default().max_nozzle as i64;
            let curr = curr_temp.unwrap_or(*tar_temp);
            for (label, t) in [("--tar-temp", *tar_temp), ("--curr-temp", curr)] {
                if t < 0 || t > max {
                    return Err(CliError::new(
                        exit::VALIDATION,
                        format!("{label} {t}°C is out of range (0..={max})"),
                    ));
                }
            }
            let cmd = ProtoCommand::AmsChangeFilament {
                target: *tray,
                curr_temp: curr,
                tar_temp: *tar_temp,
            };
            if *dry_run {
                print_json(&cmd.to_payload("1"));
                return Ok(());
            }
            if !*confirm {
                return Err(CliError::new(
                    exit::CONFIRM_REQUIRED,
                    "ams change physically moves filament; needs --confirm (try --dry-run first)",
                ));
            }
            // A filament change is a physical operation — only when idle.
            ensure_idle(cli)?;
            let client = connect_client(cli, 30)?;
            eprintln!(
                "changing filament to tray {tray} … [spec, untested on this unit] — \
                 the ACK confirms acceptance; watch `bambu status` for the physical change"
            );
            report_command_outcome(cli, client.send_and_verify(&cmd)?)
        }
        AmsAction::SetFilament {
            ams,
            tray,
            material,
            color,
            min,
            max,
            info_idx,
            dry_run,
            confirm,
        } => {
            // Validate the user input before building the command.
            if min > max {
                return Err(CliError::new(
                    exit::VALIDATION,
                    format!("--min {min} must be <= --max {max}"),
                ));
            }
            let limit = TempLimits::default().max_nozzle as i64;
            if *min < 0 || *max > limit {
                return Err(CliError::new(
                    exit::VALIDATION,
                    format!("nozzle temps must be within 0..={limit}°C"),
                ));
            }
            if color.len() != 8 || !color.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(CliError::new(
                    exit::VALIDATION,
                    format!("--color must be 8 hex digits RRGGBBAA (got {color:?})"),
                ));
            }
            let cmd = ProtoCommand::AmsFilamentSetting(Box::new(AmsFilamentSetting {
                ams_id: *ams,
                tray_id: *tray,
                tray_info_idx: info_idx.clone(),
                tray_color: color.clone(),
                nozzle_temp_min: *min,
                nozzle_temp_max: *max,
                tray_type: material.clone(),
            }));
            if *dry_run {
                print_json(&cmd.to_payload("1"));
                return Ok(());
            }
            control(cli, cmd, *confirm, "ams set-filament")
        }
        AmsAction::Settings {
            ams,
            startup_read,
            tray_read,
            confirm,
        } => control(
            cli,
            ProtoCommand::AmsUserSetting {
                ams_id: *ams,
                startup_read: *startup_read,
                tray_read: *tray_read,
            },
            *confirm,
            "ams settings",
        ),
    }
}

fn run_calibrate(cli: &Cli, action: &CalibrateAction) -> Result<(), CliError> {
    let (bed_level, vibration, motor_noise, args) = match action {
        CalibrateAction::BedLevel(a) => (true, false, false, a),
        CalibrateAction::Vibration(a) => (false, true, false, a),
        CalibrateAction::MotorNoise(a) => (false, false, true, a),
        CalibrateAction::Auto(a) => (true, true, false, a),
    };
    let cmd = ProtoCommand::Calibration {
        bed_level,
        vibration,
        motor_noise,
    };
    let what = describe_calibration(bed_level, vibration, motor_noise);

    if args.dry_run {
        // Human-readable by default; JSON only with --json (matches the contract).
        if want_json(cli) {
            print_json(&serde_json::json!({
                "plan": {
                    "bed_level": bed_level,
                    "vibration": vibration,
                    "motor_noise": motor_noise,
                    "what": what,
                },
                "payload": cmd.to_payload("1"),
            }));
        } else {
            eprintln!("dry run — would run calibration: {what}");
            eprintln!("(nothing sent; re-run with --confirm to start)");
        }
        return Ok(());
    }
    if !args.confirm {
        return Err(CliError::new(
            exit::CONFIRM_REQUIRED,
            "calibration moves the hardware; needs --confirm (try --dry-run first)",
        ));
    }
    ensure_idle(cli)?;
    let client = connect_client(cli, 20)?;
    eprintln!("starting calibration: {what} …");
    let outcome = client.send_and_verify(&cmd)?;
    // With --watch, follow the report to completion (like `job start --watch`);
    // otherwise the accept/verify verdict is the result.
    if args.watch && outcome == CommandOutcome::Verified {
        eprintln!("calibration started; watching until it finishes …");
        let (model, profile_name) = watch_identity(cli)?;
        let watcher = connect_client(cli, args.watch_timeout)?;
        let watch_interval = args.interval.map(Duration::from_secs);
        watch_to_terminal(
            &watcher,
            cli,
            model,
            profile_name,
            false,
            watch_interval,
            false,
        )
    } else {
        report_command_outcome(cli, outcome)
    }
}

/// A human label for the calibration steps that are enabled.
fn describe_calibration(bed_level: bool, vibration: bool, motor_noise: bool) -> String {
    let mut parts = Vec::new();
    if bed_level {
        parts.push("bed level");
    }
    if vibration {
        parts.push("vibration");
    }
    if motor_noise {
        parts.push("motor noise");
    }
    if parts.is_empty() {
        "nothing".to_string()
    } else {
        parts.join(" + ")
    }
}

fn job_control(cli: &Cli, cmd: ProtoCommand, confirm: bool) -> Result<(), CliError> {
    if !confirm {
        return Err(CliError::new(
            exit::CONFIRM_REQUIRED,
            "this control command needs --confirm",
        ));
    }
    let client = connect_client(cli, 15)?;
    report_command_outcome(cli, client.send_and_verify(&cmd)?)
}

fn run_camera(cli: &Cli, action: &CameraAction) -> Result<(), CliError> {
    match action {
        CameraAction::Snapshot { out, timeout } => {
            let camera =
                CameraClient::new(resolve_target(cli)?).with_timeout(Duration::from_secs(*timeout));
            let jpeg = camera.snapshot()?;
            std::fs::write(out, &jpeg).map_err(|e| {
                CliError::new(exit::GENERAL, format!("write {}: {e}", out.display()))
            })?;
            eprintln!("wrote {} bytes", jpeg.len());
            if want_json(cli) {
                print_json(&serde_json::json!({
                    "path": out.to_string_lossy(),
                    "bytes": jpeg.len(),
                }));
            } else {
                // The file path is the result (never inline image bytes).
                println!("{}", out.display());
            }
            Ok(())
        }
    }
}

fn run_timelapse(cli: &Cli, action: &TimelapseAction) -> Result<(), CliError> {
    match action {
        TimelapseAction::Enable { timeout } => {
            timelapse_set(cli, TimelapseControl::Enable, *timeout)
        }
        TimelapseAction::Disable { timeout } => {
            timelapse_set(cli, TimelapseControl::Disable, *timeout)
        }
        TimelapseAction::List => {
            let names = FtpsClient::new(resolve_target(cli)?).list("/timelapse")?;
            if want_json(cli) {
                print_json(&names);
            } else if names.is_empty() {
                println!("no timelapse files on the printer");
            } else {
                for n in &names {
                    println!("{n}");
                }
            }
            Ok(())
        }
        TimelapseAction::Get { name, out } => {
            // Accept either a bare file name or a full on-printer path.
            let remote = if name.starts_with('/') {
                name.clone()
            } else {
                format!("/timelapse/{name}")
            };
            let local = match out {
                Some(p) => p.clone(),
                None => std::path::Path::new(&remote)
                    .file_name()
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| {
                        CliError::new(exit::VALIDATION, "cannot derive an output name; pass --out")
                    })?,
            };
            let n = FtpsClient::new(resolve_target(cli)?).download(&remote, &local)?;
            eprintln!("downloaded {n} bytes to {}", local.display());
            if want_json(cli) {
                print_json(&serde_json::json!({
                    "path": local.to_string_lossy(),
                    "bytes": n,
                }));
            } else {
                println!("{}", local.display());
            }
            Ok(())
        }
        TimelapseAction::Capture {
            on_layer_cmd,
            out_dir,
            every,
            ext,
            interval,
            timeout,
            wait,
        } => run_timelapse_capture(
            cli,
            on_layer_cmd,
            out_dir,
            *every,
            ext,
            interval.map(Duration::from_secs),
            *timeout,
            *wait,
        ),
    }
}

fn timelapse_set(cli: &Cli, control: TimelapseControl, timeout_secs: u64) -> Result<(), CliError> {
    let client = connect_client(cli, timeout_secs)?;
    eprintln!("setting timelapse {} …", control.as_str());
    report_command_outcome(
        cli,
        client.send_and_verify(&ProtoCommand::IpcamTimelapse(control))?,
    )
}

/// Drive an external camera: watch the active print and run a capture command on
/// each new layer. This is the workaround for a missing/broken built-in camera —
/// the printer's own `layer_num` is the trigger; the user supplies any capture
/// tool. Capture runs as argv (no shell) with `{frame}`/`{layer}`/`{outdir}`
/// substituted; a failed grab is logged and skipped so it never aborts the watch.
// A CLI handler fanning out one flag per parameter — grouping them into a struct
// would add indirection without making the call site (a single match arm) clearer.
#[allow(clippy::too_many_arguments)]
fn run_timelapse_capture(
    cli: &Cli,
    on_layer_cmd: &[String],
    out_dir: &std::path::Path,
    every: u64,
    ext: &str,
    interval: Option<Duration>,
    timeout_secs: u64,
    wait: bool,
) -> Result<(), CliError> {
    if every == 0 {
        return Err(CliError::new(exit::VALIDATION, "--every must be >= 1"));
    }
    if ext.is_empty() || ext.len() > 12 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(CliError::new(
            exit::VALIDATION,
            "--ext must be 1-12 alphanumeric characters (e.g. jpg, png)",
        ));
    }
    std::fs::create_dir_all(out_dir)
        .map_err(|e| CliError::new(exit::GENERAL, format!("create {}: {e}", out_dir.display())))?;
    let client = connect_client(cli, timeout_secs)?;

    if wait {
        eprintln!(
            "waiting for a print to start, then capturing every {} layer(s) to {} …",
            every,
            out_dir.display()
        );
    } else {
        eprintln!(
            "watching the active print; capturing every {} layer(s) to {} …",
            every,
            out_dir.display()
        );
    }

    // Run captures on a dedicated worker thread fed by a channel, so a slow
    // capture command never blocks the MQTT event loop (which would miss layer
    // updates and risk tripping the keepalive). The watch callback only enqueues.
    let (tx, rx) = std::sync::mpsc::channel::<(std::path::PathBuf, i64)>();
    let worker = {
        let argv = on_layer_cmd.to_vec();
        let dir = out_dir.to_path_buf();
        std::thread::spawn(move || {
            let (mut captured, mut failures) = (0u64, 0u64);
            for (frame, layer) in rx {
                match run_capture_cmd(&argv, &frame, layer, &dir) {
                    Ok(()) => {
                        captured += 1;
                        eprintln!("captured frame (layer {layer}) -> {}", frame.display());
                    }
                    Err(e) => {
                        failures += 1;
                        eprintln!("capture failed at layer {layer}: {e} (continuing)");
                    }
                }
            }
            (captured, failures)
        })
    };

    // Only capture while a print is actually progressing — otherwise an idle
    // printer's stale `layer_num` (often 0) would trigger a spurious frame.
    let is_active = |s: Option<GcodeState>| {
        matches!(
            s,
            Some(GcodeState::Running | GcodeState::Prepare | GcodeState::Pause)
        )
    };
    // Scope the callback so its borrow of `tx` ends before we drop `tx` (which
    // signals the worker to finish and lets us join it for the final counts).
    let watch_result = {
        let mut last_layer: Option<i64> = None;
        let mut frame_no: u64 = 0;
        // `--wait` keys off this: until the print has been active at least once,
        // an idle/finished/error state must not end the watch.
        let mut seen_active = false;
        let mut on_update = |state: &ReportState| -> WatchStep {
            let st = PrinterStatus::from_state(state.get());
            if is_active(st.state()) {
                seen_active = true;
                if let Some(layer) = st.layer_num
                    && last_layer != Some(layer)
                {
                    last_layer = Some(layer);
                    // Capture on every Nth layer (layer 0 = the first reported layer).
                    if layer >= 0 && (layer as u64).is_multiple_of(every) {
                        frame_no += 1;
                        let frame = out_dir.join(format!("frame_{frame_no:06}_layer_{layer:05}.{ext}"));
                        // Enqueue; the worker captures. send fails only if the worker
                        // died, which we surface via the join below.
                        let _ = tx.send((frame, layer));
                    }
                }
            }
            capture_watch_step(wait, seen_active, st.state(), st.error.is_some())
        };
        client.watch(interval, &mut on_update)
    };
    // Close the channel and drain the worker (runs any queued captures), then
    // read the tallies it accumulated.
    drop(tx);
    let (captured, failures) = worker.join().unwrap_or((0, 0));

    let ended_by = match &watch_result {
        Ok(_) => "terminal",
        Err(ClientError::Timeout(_)) => "timeout",
        Err(_) => "error",
    };
    // A hard transport error (not a stall) is still a failure to report.
    if let Err(e) = watch_result
        && !matches!(e, ClientError::Timeout(_))
    {
        return Err(e.into());
    }

    eprintln!("done: {captured} frame(s) captured, {failures} failure(s) ({ended_by})");
    let suggested = ffmpeg_suggestion(out_dir, ext);
    if want_json(cli) {
        print_json(&serde_json::json!({
            "captured": captured,
            "failures": failures,
            "out_dir": out_dir.to_string_lossy(),
            "ended_by": ended_by,
            "suggested_assemble": (captured > 0).then_some(suggested.clone()),
        }));
    }
    if captured == 0 {
        eprintln!(
            "no frames captured — start this during an active print (the printer \
             must be RUNNING and advancing layers), or pass --wait to launch it \
             first and have it wait for the print to start."
        );
        return Ok(());
    }
    // Frames are written; stitching is left to the user (avoids a second
    // command-with-flags arg, and ffmpeg invocations vary). Print the suggestion.
    if !want_json(cli) {
        println!("to build a video:\n  {suggested}");
    }
    Ok(())
}

/// Decide whether the capture watch should stop. Pure so the (otherwise
/// closure-embedded) end-of-watch logic is table-testable.
///
/// With `--wait`, the watch sits through idle / finished-from-last-print / even a
/// stale error state until the print has actually been active at least once — so
/// you can launch the capture *before* starting the print and it just waits. Once
/// a print has been seen active, a terminal state or an error ends the watch, as
/// it always did. Without `--wait`, the print must already be running: any
/// terminal/error stops immediately.
fn capture_watch_step(
    wait: bool,
    seen_active: bool,
    state: Option<GcodeState>,
    has_error: bool,
) -> WatchStep {
    // While waiting for the print to start, nothing ends the watch.
    if wait && !seen_active {
        return WatchStep::Continue;
    }
    if has_error {
        return WatchStep::Stop;
    }
    match state {
        Some(s) if is_watch_terminal(s) => WatchStep::Stop,
        _ => WatchStep::Continue,
    }
}

/// A suggested `ffmpeg` line to stitch the frames (glob handles the layer suffix
/// in frame names; sequential `frame_NNNNNN` keeps them ordered).
fn ffmpeg_suggestion(out_dir: &std::path::Path, ext: &str) -> String {
    let dir = out_dir.display();
    format!(
        "ffmpeg -framerate 12 -pattern_type glob -i '{dir}/frame_*.{ext}' \
         -c:v libx264 -pix_fmt yuv420p {dir}/timelapse.mp4"
    )
}

/// Substitute the capture-command tokens in one argv element. Pure so the
/// (security-relevant) substitution is unit-testable; values land in distinct
/// argv elements and are never re-parsed by a shell.
fn subst_capture_tokens(s: &str, frame: &str, layer: i64, out_dir: &str) -> String {
    s.replace("{frame}", frame)
        .replace("{layer}", &layer.to_string())
        .replace("{outdir}", out_dir)
}

/// Run one capture command (argv, no shell), substituting frame/layer/outdir.
fn run_capture_cmd(
    argv: &[String],
    frame: &std::path::Path,
    layer: i64,
    out_dir: &std::path::Path,
) -> Result<(), String> {
    let frame = frame.to_string_lossy();
    let dir = out_dir.to_string_lossy();
    let subst = |s: &str| subst_capture_tokens(s, &frame, layer, &dir);
    let prog = subst(&argv[0]);
    let args: Vec<String> = argv[1..].iter().map(|a| subst(a)).collect();
    let status = std::process::Command::new(&prog)
        .args(&args)
        .status()
        .map_err(|e| format!("spawn {prog:?}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{prog:?} exited with {status}"))
    }
}

/// Resolve a connection target from the selected profile + overrides.
fn resolve_target(cli: &Cli) -> Result<ResolvedTarget, CliError> {
    let cfg = Config::load_or_default(&config_path()?)?;
    let profile = selected_profile_name(cli, &cfg)?.and_then(|n| cfg.profile(&n).cloned());
    let overrides = flag_overrides(cli).over(Overrides::from_env());
    Ok(config::resolve(profile.as_ref(), &overrides)?)
}

/// Resolve the target and build a client with the given timeout (shared setup
/// for control commands).
fn connect_client(cli: &Cli, timeout_secs: u64) -> Result<LanMqttClient, CliError> {
    Ok(LanMqttClient::new(resolve_target(cli)?).with_timeout(Duration::from_secs(timeout_secs)))
}

/// Map a control command's verification outcome to output + an exit code.
///
/// Under `--json` the outcome is emitted to stdout as a stable object for every
/// variant (so an agent gets a machine-readable verdict on writes, not just
/// reads); the exit code is unchanged. Without `--json` the verdict is the exit
/// code plus a human line (stderr).
fn report_command_outcome(cli: &Cli, outcome: CommandOutcome) -> Result<(), CliError> {
    if want_json(cli) {
        let v = match &outcome {
            CommandOutcome::Verified => serde_json::json!({ "outcome": "verified" }),
            CommandOutcome::Rejected { reason } => {
                serde_json::json!({ "outcome": "rejected", "reason": reason })
            }
            CommandOutcome::Unverified { stage } => serde_json::json!({
                "outcome": "unverified",
                "stage": match stage {
                    VerifyStage::Ack => "ack",
                    VerifyStage::Effect => "effect",
                },
            }),
        };
        print_json(&v);
    }
    match outcome {
        CommandOutcome::Verified => {
            if !want_json(cli) {
                eprintln!("verified: the printer confirmed the command took effect");
            }
            Ok(())
        }
        CommandOutcome::Rejected { reason } => Err(CliError::new(
            exit::DEVICE_REJECTED,
            format!("the printer rejected the command: {reason}"),
        )),
        CommandOutcome::Unverified {
            stage: VerifyStage::Ack,
        } => Err(CliError::new(
            exit::VERIFY_TIMEOUT,
            "command published but not acknowledged within the timeout (unverified)",
        )),
        CommandOutcome::Unverified {
            stage: VerifyStage::Effect,
        } => Err(CliError::new(
            exit::VERIFY_TIMEOUT,
            "command was acknowledged but its effect never showed in the report \
             (the printer's state didn't change — e.g. a print that won't start \
             or a light that won't switch); unverified — check `bambu status`",
        )),
    }
}

/// A print is "done" for watching once it finishes, fails, or returns to idle.
fn is_watch_terminal(state: GcodeState) -> bool {
    matches!(
        state,
        GcodeState::Finish | GcodeState::Failed | GcodeState::Idle
    )
}

/// Resolve which profile to use: explicit `--printer`, else the configured
/// default. Returns `None` when neither is set (the caller then relies on
/// flag/env overrides). A name that IS set but is not in the config is an error
/// — we never silently fall back to a different target (e.g. on a `--printer`
/// typo with `BAMBU_*` in the environment).
fn selected_profile_name(cli: &Cli, cfg: &Config) -> Result<Option<String>, CliError> {
    let name = match cli.printer.clone().or_else(|| cfg.default_printer.clone()) {
        Some(n) => n,
        None => return Ok(None),
    };
    if cfg.printers.contains_key(&name) {
        Ok(Some(name))
    } else {
        Err(CliError::from(ConfigError::UnknownProfile(name)))
    }
}

/// JSON output is the default when stdout is not a TTY, or when `--json` is set.
fn want_json(cli: &Cli) -> bool {
    // Output is human-readable by default and JSON only with an explicit
    // `--json` — no TTY auto-detection (that magic surprised users piping into
    // e.g. `watch`). Agents/scripts pass `--json`; matches `gh`'s convention.
    cli.json
}

fn flag_overrides(cli: &Cli) -> Overrides {
    Overrides {
        ip: cli.ip.clone(),
        serial: cli.serial.clone(),
        access_code: cli.access_code.clone(),
        model: cli.model.clone(),
    }
}

fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: failed to serialize output: {e}"),
    }
}

fn print_status_human(o: &StatusOutput) {
    let s = &o.status;
    println!(
        "printer: {} ({})",
        o.printer.as_deref().unwrap_or("-"),
        o.model
    );
    println!("state:   {}", s.gcode_state.as_deref().unwrap_or("?"));
    // A device-level fault (print_error) is the most important thing to see.
    if let Some(err) = &s.error {
        println!("error:   ⚠ {} (print_error {})", err.hex, err.code);
        println!("         {}", err.lookup_url);
    }
    // Show the current activity only when it's a real special stage; the
    // no-stage markers (0 / 255) just echo idle-or-printing.
    if let (Some(stage), Some(id)) = (s.stage, s.stg_cur)
        && !Stage(id).is_no_stage()
    {
        println!("stage:   {stage} ({id})");
    }
    if let Some(f) = &s.filament {
        let name = f.name.as_deref().or(f.material.as_deref()).unwrap_or("?");
        let color = f
            .color
            .as_deref()
            .map(|c| format!(" #{c}"))
            .unwrap_or_default();
        println!("filament: {name} @ {}{color}", f.location);
    }
    if let (Some(n), Some(b)) = (s.nozzle_temper, s.bed_temper) {
        println!("temps:   nozzle {n:.1}°C / bed {b:.1}°C");
    }
    if let Some(tl) = s.timelapse_mode() {
        println!("timelapse: {tl}");
    }
    if let Some(lvl) = s.spd_lvl {
        let name = SpeedLevel::from_level(lvl)
            .map(|l| l.as_str())
            .unwrap_or("?");
        println!("speed:   {name} ({lvl})");
    }
    if let Some(p) = s.mc_percent {
        let layer = s.layer_num.unwrap_or(0);
        let total = s.total_layer_num.unwrap_or(0);
        let eta = match s.remaining_time_min.filter(|m| *m > 0) {
            Some(m) => format!(", ETA {}", fmt_eta(m)),
            None => String::new(),
        };
        println!("progress: {p}% (layer {layer}/{total}{eta})");
    }
}

/// Format a remaining-time estimate in minutes as `15m` or `1h35m`.
fn fmt_eta(min: i64) -> String {
    if min >= 60 {
        format!("{}h{:02}m", min / 60, min % 60)
    } else {
        format!("{min}m")
    }
}

#[cfg(test)]
mod tests {
    use super::{ams_mapping_preview, fmt_eta, subst_capture_tokens, validate_ams_map};

    #[test]
    fn capture_watch_waits_for_print_then_ends_at_terminal() {
        use super::{capture_watch_step, GcodeState, WatchStep};
        let stop = |s: WatchStep| matches!(s, WatchStep::Stop);
        let go = |s: WatchStep| matches!(s, WatchStep::Continue);

        // Without --wait: legacy behaviour — any terminal/idle state ends the
        // watch at once; an active print keeps it going.
        assert!(stop(capture_watch_step(false, false, Some(GcodeState::Idle), false)));
        assert!(stop(capture_watch_step(false, false, Some(GcodeState::Finish), false)));
        assert!(go(capture_watch_step(false, false, Some(GcodeState::Running), false)));

        // With --wait, before any active state: nothing ends the watch — not idle,
        // not a finished-from-last-print state, not even a stale print error.
        assert!(go(capture_watch_step(true, false, Some(GcodeState::Idle), false)));
        assert!(go(capture_watch_step(true, false, Some(GcodeState::Finish), false)));
        assert!(go(capture_watch_step(true, false, Some(GcodeState::Finish), true)));

        // With --wait, once the print has been active: a terminal state or an
        // error ends it, exactly like the legacy path.
        assert!(go(capture_watch_step(true, true, Some(GcodeState::Running), false)));
        assert!(stop(capture_watch_step(true, true, Some(GcodeState::Finish), false)));
        assert!(stop(capture_watch_step(true, true, Some(GcodeState::Running), true)));
    }

    #[test]
    fn eta_formats_minutes_and_hours() {
        assert_eq!(fmt_eta(15), "15m");
        assert_eq!(fmt_eta(59), "59m");
        assert_eq!(fmt_eta(60), "1h00m");
        assert_eq!(fmt_eta(95), "1h35m");
    }

    #[test]
    fn ams_map_range_is_always_checked() {
        // -1..=3 are fine (count unknown).
        assert!(validate_ams_map(&[0, 3, -1], None).is_ok());
        // Out of range -> error even without a filament count.
        assert!(validate_ams_map(&[0, 4], None).is_err());
        assert!(validate_ams_map(&[-2], None).is_err());
    }

    #[test]
    fn ams_map_length_must_match_filament_count_when_known() {
        // 2 filaments, 2 entries -> ok.
        assert!(validate_ams_map(&[0, 1], Some(2)).is_ok());
        // 2 entries but 3 filaments -> error.
        assert!(validate_ams_map(&[0, 1], Some(3)).is_err());
        // 1 entry, 2 filaments -> error (the classic footgun).
        assert!(validate_ams_map(&[0], Some(2)).is_err());
    }

    #[test]
    fn ams_map_warns_on_multiple_external_spools() {
        let warns = validate_ams_map(&[-1, -1], Some(2)).unwrap();
        assert!(warns.iter().any(|w| w.contains("external spool")));
        // A single -1 is fine, no warning.
        assert!(validate_ams_map(&[0, -1], Some(2)).unwrap().is_empty());
    }

    #[test]
    fn ams_preview_pairs_filaments_with_trays() {
        let colors = vec!["#F2754E".to_string(), "#0000FF".to_string()];
        let v = ams_mapping_preview(&colors, &[2, -1]);
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0]["color"], "#F2754E");
        assert_eq!(arr[0]["tray"], 2);
        assert_eq!(arr[0]["source"], "AMS tray 2");
        assert_eq!(arr[1]["tray"], -1);
        assert_eq!(arr[1]["source"], "external spool");
    }

    #[test]
    fn capture_tokens_substitute_per_argv_element() {
        assert_eq!(
            subst_capture_tokens("{outdir}/f_{layer}.jpg", "/t/frame.jpg", 42, "/t"),
            "/t/f_42.jpg"
        );
        assert_eq!(
            subst_capture_tokens("{frame}", "/t/frame.jpg", 7, "/t"),
            "/t/frame.jpg"
        );
        // No tokens -> unchanged.
        assert_eq!(subst_capture_tokens("-r", "/f.jpg", 1, "/t"), "-r");
    }

    #[test]
    fn capture_tokens_do_not_interpret_shell_metacharacters() {
        // The substituted value lands verbatim in a single argv element (no
        // shell parses it), so metacharacters are inert — documents that
        // `bambu timelapse capture` runs argv directly, not via a shell.
        let layer_with_meta = subst_capture_tokens("{frame}", "/t/a b;rm -rf $HOME.jpg", 1, "/t");
        assert_eq!(layer_with_meta, "/t/a b;rm -rf $HOME.jpg");
    }
}

#[derive(Serialize)]
struct StatusOutput {
    printer: Option<String>,
    model: String,
    #[serde(flatten)]
    status: PrinterStatus,
}

/// A profile view with the access code redacted, for `config show`.
#[derive(Serialize)]
struct RedactedProfile<'a> {
    name: &'a str,
    ip: &'a str,
    serial: &'a str,
    model: &'a str,
    mode: &'a str,
    access_code: &'static str,
}

impl<'a> RedactedProfile<'a> {
    fn from(name: &'a str, p: &'a Profile) -> Self {
        Self {
            name,
            ip: &p.ip,
            serial: &p.serial,
            model: &p.model,
            mode: &p.mode,
            access_code: "<redacted>",
        }
    }
}

impl std::fmt::Display for RedactedProfile<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: ip={} serial={} model={} mode={} access_code={}",
            self.name, self.ip, self.serial, self.model, self.mode, self.access_code
        )
    }
}
