//! The `bambu` command-line interface (behind the `cli` feature).
//!
//! Thin layer over the library: parse args, resolve a connection target, call
//! the client, format output. Agent contract: JSON to stdout (default when
//! stdout is not a TTY, or with `--json`), human text otherwise; a semantic
//! exit-code scheme; the access code is never printed.

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::client::{ClientError, LanMqttClient, StatusSource};
use crate::config::{self, Config, ConfigError, Overrides, Profile};
use crate::core::status::PrinterStatus;

/// Exit codes (a subset of the documented scheme).
mod exit {
    pub const GENERAL: u8 = 1;
    pub const VALIDATION: u8 = 3;
    pub const VERIFY_TIMEOUT: u8 = 6;
    pub const TRANSPORT: u8 = 7;
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
    /// Emit JSON (the default when stdout is not a TTY).
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
    /// Fetch and print a status snapshot.
    Status,
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

/// Entry point. Parses args, dispatches, and maps errors to exit codes.
pub fn run() -> ExitCode {
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
        Command::Status => run_status(cli),
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
            if cli.json {
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
            let name = selected_profile_name(cli, &cfg)?;
            let profile = cfg
                .profile(&name)
                .ok_or_else(|| CliError::from(ConfigError::UnknownProfile(name.clone())))?;
            let view = RedactedProfile::from(&name, profile);
            if cli.json {
                print_json(&view);
            } else {
                println!("{view}");
            }
            Ok(())
        }
    }
}

fn run_status(cli: &Cli) -> Result<(), CliError> {
    let cfg = Config::load_or_default(&config_path()?)?;
    let profile_name = selected_profile_name(cli, &cfg).ok();
    let profile = profile_name.as_deref().and_then(|n| cfg.profile(n));

    let overrides = flag_overrides(cli).over(Overrides::from_env());
    let target = config::resolve(profile, &overrides)?;
    let model = target.model.clone();

    let state = LanMqttClient::new(target).fetch_snapshot()?;
    let status = PrinterStatus::from_state(state.get());

    let output = StatusOutput {
        printer: profile_name,
        model: model.to_string(),
        status,
    };

    if cli.json || !std::io::stdout().is_terminal() {
        print_json(&output);
    } else {
        print_status_human(&output);
    }
    Ok(())
}

/// Resolve which profile name to use: explicit `--printer`, else the default.
fn selected_profile_name(cli: &Cli, cfg: &Config) -> Result<String, CliError> {
    cli.printer
        .clone()
        .or_else(|| cfg.default_printer.clone())
        .ok_or_else(|| {
            CliError::new(
                exit::VALIDATION,
                "no printer selected: pass --printer or set a default",
            )
        })
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
    if let (Some(n), Some(b)) = (s.nozzle_temper, s.bed_temper) {
        println!("temps:   nozzle {n:.1}°C / bed {b:.1}°C");
    }
    if let Some(p) = s.mc_percent {
        let layer = s.layer_num.unwrap_or(0);
        let total = s.total_layer_num.unwrap_or(0);
        println!("progress: {p}% (layer {layer}/{total})");
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
