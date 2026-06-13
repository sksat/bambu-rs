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
use crate::core::command::{Command as ProtoCommand, ProjectFile};
use crate::core::stage::Stage;
use crate::core::status::{GcodeState, PrinterStatus};
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
        /// With --watch, stop after this many seconds (default 1h; or Ctrl-C).
        /// Drops are auto-reconnected within this budget.
        #[arg(long, default_value_t = 3600)]
        timeout: u64,
    },
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
    /// Turn the chamber/work light on or off (control test; low-risk).
    Light {
        /// "on" or "off".
        #[arg(value_parser = ["on", "off"])]
        state: String,
        /// Watch the report for this many seconds after sending.
        #[arg(long, default_value_t = 8)]
        timeout: u64,
    },
    /// Run printer calibration (bed leveling / vibration / motor noise).
    Calibrate {
        /// Bed leveling.
        #[arg(long)]
        bed_level: bool,
        /// Vibration compensation.
        #[arg(long)]
        vibration: bool,
        /// Motor-noise calibration.
        #[arg(long)]
        motor_noise: bool,
        /// Show the resolved command JSON without sending it (safe).
        #[arg(long)]
        dry_run: bool,
        /// Required to actually run calibration (it moves the hardware).
        #[arg(long)]
        confirm: bool,
    },
    /// Send a raw G-code line and watch the report (control; needs --confirm).
    Gcode {
        /// The G-code line, e.g. "G28" (home all axes).
        line: String,
        /// Required to actually send a control command.
        #[arg(long)]
        confirm: bool,
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
        /// Show the resolved command JSON without sending it (safe).
        #[arg(long)]
        dry_run: bool,
        /// Required to actually start a print.
        #[arg(long)]
        confirm: bool,
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
}

/// A CLI error carrying the exit code to return.
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
        Command::Hms => run_hms(cli),
        Command::Job { action } => run_job(cli, action),
        Command::File { action } => run_file(cli, action),
        Command::Camera { action } => run_camera(cli, action),
        Command::Light { state, timeout } => run_light(cli, state == "on", *timeout),
        Command::Calibrate {
            bed_level,
            vibration,
            motor_noise,
            dry_run,
            confirm,
        } => run_calibrate(
            cli,
            *bed_level,
            *vibration,
            *motor_noise,
            *dry_run,
            *confirm,
        ),
        Command::Gcode {
            line,
            confirm,
            timeout,
        } => run_gcode(cli, line, *confirm, *timeout),
        Command::Reboot { confirm } => run_reboot(cli, *confirm),
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
    // A continuous monitor reconnects through drops; a to-terminal watch fails fast.
    let result = client.watch(interval, continuous, |state| {
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
    });

    let final_state = match result {
        Ok(fs) => fs,
        // The continuous monitor ends only via its timeout (or Ctrl-C): not a failure.
        Err(ClientError::Timeout(_)) if continuous => return Ok(()),
        Err(e) => return Err(e.into()),
    };

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

fn run_light(cli: &Cli, on: bool, timeout_secs: u64) -> Result<(), CliError> {
    let client = connect_client(cli, timeout_secs)?;
    eprintln!("setting chamber_light {} …", if on { "on" } else { "off" });
    report_command_outcome(client.send_and_verify(&ProtoCommand::ChamberLight(on))?)
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

fn run_gcode(cli: &Cli, line: &str, confirm: bool, timeout_secs: u64) -> Result<(), CliError> {
    if !confirm {
        return Err(CliError::new(
            exit::CONFIRM_REQUIRED,
            "refusing to send a control command without --confirm",
        ));
    }
    let client = connect_client(cli, timeout_secs)?;
    eprintln!("sending gcode_line {line:?} …");
    report_command_outcome(client.send_and_verify(&ProtoCommand::GcodeLine(line.to_string()))?)
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
    }
}

fn run_job(cli: &Cli, action: &JobAction) -> Result<(), CliError> {
    match action {
        JobAction::Start {
            file,
            plate,
            ams_map,
            bed_type,
            dry_run,
            confirm,
            watch,
            watch_timeout,
            interval,
        } => {
            let cmd = build_start_command(file, *plate, ams_map.as_deref(), bed_type)?;
            if *dry_run {
                // Show the resolved payload (the "plan") without sending anything.
                print_json(&cmd.to_payload("1"));
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
                report_command_outcome(outcome)
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

fn run_calibrate(
    cli: &Cli,
    bed_level: bool,
    vibration: bool,
    motor_noise: bool,
    dry_run: bool,
    confirm: bool,
) -> Result<(), CliError> {
    // Default to the common A1 calibration (bed leveling + vibration) when no
    // step is requested.
    let (bed_level, vibration) = if !bed_level && !vibration && !motor_noise {
        (true, true)
    } else {
        (bed_level, vibration)
    };
    let cmd = ProtoCommand::Calibration {
        bed_level,
        vibration,
        motor_noise,
    };
    if dry_run {
        print_json(&cmd.to_payload("1"));
        return Ok(());
    }
    if !confirm {
        return Err(CliError::new(
            exit::CONFIRM_REQUIRED,
            "calibration moves the hardware; needs --confirm (try --dry-run first)",
        ));
    }
    ensure_idle(cli)?;
    let client = connect_client(cli, 20)?;
    eprintln!(
        "starting calibration (bed_level={bed_level} vibration={vibration} motor_noise={motor_noise}) …"
    );
    report_command_outcome(client.send_and_verify(&cmd)?)
}

fn job_control(cli: &Cli, cmd: ProtoCommand, confirm: bool) -> Result<(), CliError> {
    if !confirm {
        return Err(CliError::new(
            exit::CONFIRM_REQUIRED,
            "this control command needs --confirm",
        ));
    }
    let client = connect_client(cli, 15)?;
    report_command_outcome(client.send_and_verify(&cmd)?)
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
fn report_command_outcome(outcome: CommandOutcome) -> Result<(), CliError> {
    match outcome {
        CommandOutcome::Verified => {
            eprintln!("verified: the printer confirmed the command took effect");
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
            "command was acknowledged but its effect was never observed \
             (e.g. the print did not start); unverified — check `bambu status`",
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
    if let (Some(stage), Some(id)) = (s.stage, s.stg_cur) {
        if !Stage(id).is_no_stage() {
            println!("stage:   {stage} ({id})");
        }
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
    use super::fmt_eta;

    #[test]
    fn eta_formats_minutes_and_hours() {
        assert_eq!(fmt_eta(15), "15m");
        assert_eq!(fmt_eta(59), "59m");
        assert_eq!(fmt_eta(60), "1h00m");
        assert_eq!(fmt_eta(95), "1h35m");
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
