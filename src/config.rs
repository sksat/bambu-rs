//! Printer connection profiles and credential resolution.
//!
//! A [`Config`] holds named [`Profile`]s on disk; [`resolve`] merges a profile
//! with per-invocation [`Overrides`] (flags / `BAMBU_*` env) — overrides win —
//! into a [`ResolvedTarget`] ready to connect with. The LAN access code is a
//! secret: it is stored 0600, never logged, and redacted from `Debug`.
//!
//! (An OS-keyring backend is a planned enhancement; this is the 0600-file
//! fallback the plan calls for.)

use crate::core::model::Model;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A stored printer profile.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub ip: String,
    pub serial: String,
    /// Canonical model name (see [`Model::from_config_str`]).
    pub model: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    /// LAN access code (the 8-digit secret). Redacted from `Debug`.
    pub access_code: String,
    /// Named `.gcode` sequences for THIS printer: `name -> path`.
    ///
    /// A control macro is per-machine, not per-model — the coordinates depend on
    /// what is bolted to that specific unit. A plate changer's swap motion is a
    /// fixed run of G-code its vendor ships, so naming the file here is all the
    /// support such an accessory needs; nothing about it belongs in this crate.
    ///
    /// Deliberately no defaults: the right values depend on the physical setup,
    /// so an unknown name is an error rather than a guess.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sequences: BTreeMap<String, String>,
    /// Sequences to run automatically around a print.
    #[serde(default, skip_serializing_if = "Hooks::is_empty")]
    pub hooks: Hooks,
}

/// Sequences fired automatically by `job start`, by the NAME of a sequence
/// defined above — not a path. Naming the sequence means the same macro is
/// usable by hand and automatically, and a hook pointing at something that
/// doesn't exist is caught rather than silently skipped.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hooks {
    /// Run just before the print command is sent.
    ///
    /// For a plate changer this is where a swap goes: ejecting at the START of
    /// the next print rather than the end of the previous one gives continuous
    /// printing with no post-print machinery, and leaves the finished part on
    /// the bed until you actually ask for the next one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_print: Option<String>,
    /// Seconds to wait for `pre_print`'s motion to actually finish before
    /// refusing to start the print. Defaults to [`DEFAULT_SETTLE_SECS`].
    ///
    /// A knob rather than a fixed constant because a macro's runtime is a
    /// property of the machine it drives — a plate swap takes ~90 s, but a
    /// sequence that heats or shuffles several plates can legitimately run
    /// longer, and there is no way to run such a hook at all if the ceiling is
    /// fixed. It has a default, unlike the tuning constants this codebase
    /// refuses to default, because a wrong value here cannot cause an unsafe
    /// action: too small only turns into a refusal to print.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_print_timeout_secs: Option<u64>,
    // TODO: `post_print`. Firing after a print needs something to still be
    // running when it ends — the CLI only is during `--watch`, so a dropped
    // ssh session would silently skip it, and a print started from the
    // printer's own screen has no bambu-rs process at all. That wants the
    // async job infrastructure `serve` needs anyway (queue, progress, cancel);
    // doing it in the CLI first would be a second mechanism to throw away.
    // Note a pre-print swap already covers the continuous-print case.
}

/// Seconds a pre-print sequence's motion may take before the wait gives up.
///
/// Generous on purpose: the verified plate swap takes ~90 s, and the cost of
/// being too generous is a slow failure, while being too tight refuses a
/// legitimate print. Shared with `gcode --wait-timeout` so the two cannot drift.
pub const DEFAULT_SETTLE_SECS: u64 = 600;

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.pre_print.is_none() && self.pre_print_timeout_secs.is_none()
    }

    /// How long to wait for `pre_print`'s motion, config or default.
    pub fn pre_print_timeout(&self) -> Duration {
        Duration::from_secs(self.pre_print_timeout_secs.unwrap_or(DEFAULT_SETTLE_SECS))
    }
}

impl Profile {
    /// Look up one of this printer's named sequences.
    ///
    /// The error lists what IS defined: the caller is usually an agent or a
    /// script that guessed a name, and "swap not found" alone leaves it to
    /// guess again.
    pub fn sequence(&self, name: &str) -> Result<&str, ConfigError> {
        self.sequences
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| ConfigError::NoSuchSequence {
                name: name.to_string(),
                available: if self.sequences.is_empty() {
                    "none defined for this profile".to_string()
                } else {
                    self.sequences
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                },
            })
    }
}

/// Resolve a configured sequence path.
///
/// - absolute → unchanged
/// - `~/…` → relative to `home` (bare prefix only; `~user` is not expanded)
/// - anything else → relative to **the config file's directory, never the cwd**
///
/// cwd-relative would make the same `--sequence swap` work in one directory and
/// fail in another — the kind of guess this crate's hard-error style exists to
/// remove — and a future `serve` under systemd runs with `cwd=/`. Anchoring to
/// the config directory also makes `~/.config/bambu-rs/sequences/swap.gcode` a
/// self-contained setup you can copy between machines.
///
/// Pure: no filesystem access. Whether the file exists is the caller's problem,
/// checked when the sequence is actually used — a dangling path in one printer's
/// profile must not break using another.
pub fn resolve_sequence_path(raw: &str, config_dir: &Path, home: Option<&Path>) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(h) = home
    {
        return h.join(rest);
    }
    config_dir.join(p)
}

fn default_mode() -> String {
    "lan".to_string()
}

// Manual Debug so the access code never leaks into logs / error output.
impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile")
            .field("ip", &self.ip)
            .field("serial", &self.serial)
            .field("model", &self.model)
            .field("mode", &self.mode)
            .field("access_code", &"<redacted>")
            .finish()
    }
}

/// The on-disk configuration: named profiles plus an optional default.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_printer: Option<String>,
    #[serde(default)]
    pub printers: BTreeMap<String, Profile>,
}

/// Errors from config handling and target resolution.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required connection field: {0}")]
    MissingField(&'static str),
    #[error("no such printer profile: {0}")]
    UnknownProfile(String),
    #[error("profile has no sequence {name:?} (defined: {available})")]
    NoSuchSequence { name: String, available: String },
    #[error("config i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
}

impl Config {
    /// Load from `path`, or return an empty config if the file doesn't exist.
    pub fn load_or_default(path: &Path) -> Result<Config, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write to `path` (creating parent dirs) with owner-only (0600) permissions.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        set_owner_only(path)?;
        Ok(())
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.printers.get(name)
    }
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}
#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The default config path (`$XDG_CONFIG_HOME/bambu-rs/config.toml`, else
/// `~/.config/bambu-rs/config.toml`).
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("bambu-rs/config.toml"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/bambu-rs/config.toml"))
}

/// Per-invocation overrides (from flags and/or env). Higher precedence than a
/// stored profile.
#[derive(Clone, Default)]
pub struct Overrides {
    pub ip: Option<String>,
    pub serial: Option<String>,
    pub access_code: Option<String>,
    pub model: Option<String>,
}

impl Overrides {
    /// Read `BAMBU_IP` / `BAMBU_SERIAL` / `BAMBU_ACCESS_CODE` / `BAMBU_MODEL`.
    pub fn from_env() -> Self {
        let v = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        Overrides {
            ip: v("BAMBU_IP"),
            serial: v("BAMBU_SERIAL"),
            access_code: v("BAMBU_ACCESS_CODE"),
            model: v("BAMBU_MODEL"),
        }
    }

    /// Overlay `self` over `lower`, `self` winning. Used to apply flags over env.
    pub fn over(self, lower: Overrides) -> Overrides {
        Overrides {
            ip: self.ip.or(lower.ip),
            serial: self.serial.or(lower.serial),
            access_code: self.access_code.or(lower.access_code),
            model: self.model.or(lower.model),
        }
    }
}

/// Parse the `BAMBU_*` assignments from `.env`-style content. Only `BAMBU_`-
/// prefixed keys are returned (so an unrelated `.env` can't inject surprising
/// config); an optional `export ` prefix and matching surrounding quotes are
/// stripped. Pure (no I/O) so it is unit-testable.
pub fn parse_dotenv(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if !k.starts_with("BAMBU_") {
            continue;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(v);
        out.push((k.to_string(), v.to_string()));
    }
    out
}

/// Best-effort: load `BAMBU_*` keys from `./.env` into the process environment,
/// **without** overriding variables already set (so the precedence stays
/// flags > real env > `.env` > config). A missing/unreadable file is ignored.
/// The access code is never logged.
pub fn load_dotenv() {
    let Ok(content) = std::fs::read_to_string(".env") else {
        return;
    };
    for (k, v) in parse_dotenv(&content) {
        if std::env::var_os(&k).is_none() {
            // Safe: called once at startup, before any threads are spawned.
            unsafe { std::env::set_var(&k, v) };
        }
    }
}

/// A fully-resolved connection target. Holds the access-code secret (redacted
/// from `Debug`).
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub ip: String,
    pub serial: String,
    pub access_code: String,
    pub model: Model,
}

impl std::fmt::Debug for ResolvedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedTarget")
            .field("ip", &self.ip)
            .field("serial", &self.serial)
            .field("model", &self.model)
            .field("access_code", &"<redacted>")
            .finish()
    }
}

/// Resolve a connection target from an optional stored profile plus overrides.
/// **Precedence: overrides win over the profile.** Every required field
/// (ip, serial, access_code, model) must come from one or the other.
pub fn resolve(
    profile: Option<&Profile>,
    overrides: &Overrides,
) -> Result<ResolvedTarget, ConfigError> {
    let pick = |ov: &Option<String>, field: fn(&Profile) -> &str, name: &'static str| {
        ov.clone()
            .or_else(|| profile.map(|p| field(p).to_string()))
            .filter(|s| !s.is_empty())
            .ok_or(ConfigError::MissingField(name))
    };
    let ip = pick(&overrides.ip, |p| &p.ip, "ip")?;
    let serial = pick(&overrides.serial, |p| &p.serial, "serial")?;
    let access_code = pick(&overrides.access_code, |p| &p.access_code, "access_code")?;
    let model_str = pick(&overrides.model, |p| &p.model, "model")?;
    Ok(ResolvedTarget {
        ip,
        serial,
        access_code,
        model: Model::from_config_str(&model_str),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dotenv_reads_bambu_keys_only_with_quotes_and_export() {
        let content = "\
# a comment
BAMBU_IP=192.0.2.10
export BAMBU_SERIAL=0309ABC
BAMBU_ACCESS_CODE=\"12345678\"
BAMBU_MODEL='a1mini'

PATH=/should/not/leak
NOT_BAMBU=ignored
malformed line without equals
";
        let got = parse_dotenv(content);
        assert_eq!(
            got,
            vec![
                ("BAMBU_IP".to_string(), "192.0.2.10".to_string()),
                ("BAMBU_SERIAL".to_string(), "0309ABC".to_string()),
                ("BAMBU_ACCESS_CODE".to_string(), "12345678".to_string()),
                ("BAMBU_MODEL".to_string(), "a1mini".to_string()),
            ]
        );
    }

    fn sample_profile() -> Profile {
        Profile {
            ip: "192.0.2.10".into(),
            serial: "0309FAxxxxxxxxx".into(),
            model: "a1mini".into(),
            mode: "lan".into(),
            access_code: "00000000".into(),
            sequences: BTreeMap::new(),
            hooks: Hooks::default(),
        }
    }

    fn profile_with_sequences() -> Profile {
        let mut p = sample_profile();
        p.sequences
            .insert("swap".into(), "sequences/swap.gcode".into());
        p.sequences
            .insert("load".into(), "sequences/load.gcode".into());
        p
    }

    #[test]
    fn an_unknown_sequence_name_lists_the_ones_that_exist() {
        // The caller is usually an agent or script that guessed. "not found"
        // alone leaves it guessing again.
        let err = profile_with_sequences().sequence("swaap").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("swaap"), "{msg}");
        assert!(msg.contains("load") && msg.contains("swap"), "{msg}");
    }

    #[test]
    fn a_profile_with_no_sequences_says_so_rather_than_listing_nothing() {
        let msg = sample_profile().sequence("swap").unwrap_err().to_string();
        assert!(msg.contains("none defined"), "{msg}");
    }

    #[test]
    fn sequence_paths_anchor_to_the_config_dir_not_the_cwd() {
        let cfg_dir = Path::new("/home/u/.config/bambu-rs");
        let home = Path::new("/home/u");
        // Absolute wins outright.
        assert_eq!(
            resolve_sequence_path("/opt/swap.gcode", cfg_dir, Some(home)),
            Path::new("/opt/swap.gcode")
        );
        // `~/` is expanded — configs are written by hand and people type it.
        assert_eq!(
            resolve_sequence_path("~/swapmod/swap.gcode", cfg_dir, Some(home)),
            Path::new("/home/u/swapmod/swap.gcode")
        );
        // The one that matters: relative resolves against the CONFIG dir, so the
        // same command works from any working directory.
        assert_eq!(
            resolve_sequence_path("sequences/swap.gcode", cfg_dir, Some(home)),
            Path::new("/home/u/.config/bambu-rs/sequences/swap.gcode")
        );
        // No home to expand against: don't invent one, leave it config-relative.
        assert_eq!(
            resolve_sequence_path("~/swap.gcode", cfg_dir, None),
            cfg_dir.join("~/swap.gcode")
        );
    }

    #[test]
    fn editing_one_field_leaves_the_rest_of_the_profile_alone() {
        // What `config set` does. The alternative — rebuilding the profile from
        // flags, as `config add` must — destroys every field the flags don't
        // cover, which is why `add` no longer overwrites.
        let mut p = profile_with_sequences();
        let before = p.clone();
        p.ip = "192.0.2.99".into();
        assert_eq!(p.ip, "192.0.2.99");
        assert_eq!(p.sequences, before.sequences);
        assert_eq!(p.serial, before.serial);
        assert_eq!(p.access_code, before.access_code);
        assert!(p.sequence("swap").is_ok());
    }

    #[test]
    fn a_hook_names_a_sequence_so_the_pair_round_trips() {
        let mut p = profile_with_sequences();
        p.hooks.pre_print = Some("swap".into());
        let cfg = Config {
            default_printer: Some("a1".into()),
            printers: BTreeMap::from([("a1".to_string(), p.clone())]),
        };
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        // A hook names a sequence, so it must resolve through the same lookup.
        let hook = back.printers["a1"].hooks.pre_print.clone().unwrap();
        assert!(back.printers["a1"].sequence(&hook).is_ok());
    }

    #[test]
    fn the_hooks_wait_budget_defaults_but_can_be_raised() {
        // A macro's runtime is a property of the machine it drives, so a hook
        // that legitimately takes longer than the default must be expressible —
        // otherwise it simply cannot be used as a hook at all.
        let mut p = profile_with_sequences();
        p.hooks.pre_print = Some("swap".into());
        assert_eq!(
            p.hooks.pre_print_timeout(),
            Duration::from_secs(DEFAULT_SETTLE_SECS)
        );

        p.hooks.pre_print_timeout_secs = Some(1800);
        assert_eq!(p.hooks.pre_print_timeout(), Duration::from_secs(1800));

        let cfg = Config {
            default_printer: Some("a1".into()),
            printers: BTreeMap::from([("a1".to_string(), p)]),
        };
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg, "the raised budget must survive a round trip");
    }

    #[test]
    fn sequences_survive_a_config_round_trip() {
        let cfg = Config {
            default_printer: Some("a1".into()),
            printers: BTreeMap::from([("a1".to_string(), profile_with_sequences())]),
        };
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn resolve_uses_profile_when_no_overrides() {
        let p = sample_profile();
        let t = resolve(Some(&p), &Overrides::default()).unwrap();
        assert_eq!(t.ip, "192.0.2.10");
        assert_eq!(t.model, Model::A1Mini);
        assert_eq!(t.access_code, "00000000");
    }

    #[test]
    fn overrides_win_over_profile() {
        let p = sample_profile();
        let ov = Overrides {
            ip: Some("198.51.100.9".into()),
            model: Some("x1c".into()),
            ..Default::default()
        };
        let t = resolve(Some(&p), &ov).unwrap();
        assert_eq!(t.ip, "198.51.100.9"); // override
        assert_eq!(t.model, Model::X1Carbon); // override
        assert_eq!(t.serial, "0309FAxxxxxxxxx"); // from profile
    }

    #[test]
    fn missing_field_is_an_error() {
        let err = resolve(None, &Overrides::default()).unwrap_err();
        assert!(matches!(err, ConfigError::MissingField("ip")));
        // Even partial overrides leave required fields missing.
        let ov = Overrides {
            ip: Some("198.51.100.9".into()),
            ..Default::default()
        };
        assert!(matches!(
            resolve(None, &ov).unwrap_err(),
            ConfigError::MissingField("serial")
        ));
    }

    #[test]
    fn overrides_over_applies_flags_above_env() {
        let env = Overrides {
            ip: Some("env-ip".into()),
            serial: Some("env-serial".into()),
            ..Default::default()
        };
        let flags = Overrides {
            ip: Some("flag-ip".into()),
            ..Default::default()
        };
        let merged = flags.over(env);
        assert_eq!(merged.ip.as_deref(), Some("flag-ip")); // flag wins
        assert_eq!(merged.serial.as_deref(), Some("env-serial")); // falls back to env
    }

    #[test]
    fn debug_redacts_the_access_code() {
        let dbg = format!("{:?}", sample_profile());
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("00000000"));
        let t = resolve(Some(&sample_profile()), &Overrides::default()).unwrap();
        assert!(!format!("{t:?}").contains("00000000"));
    }

    fn config_with_one(name: &str, default: bool) -> Config {
        let mut printers = BTreeMap::new();
        printers.insert(name.to_string(), sample_profile());
        Config {
            default_printer: default.then(|| name.to_string()),
            printers,
        }
    }

    #[test]
    fn config_toml_round_trips() {
        let cfg = config_with_one("a1", true);
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn save_then_load_round_trips_and_is_owner_only() {
        let cfg = config_with_one("a1", false);
        let path = std::env::temp_dir().join(format!(
            "bambu-rs-cfg-test-{}-{}.toml",
            std::process::id(),
            "save_load"
        ));
        cfg.save(&path).unwrap();
        let loaded = Config::load_or_default(&path).unwrap();
        assert_eq!(cfg, loaded);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_or_default_is_empty_when_absent() {
        let path = std::env::temp_dir().join("bambu-rs-definitely-not-here-9z.toml");
        let _ = std::fs::remove_file(&path);
        assert_eq!(Config::load_or_default(&path).unwrap(), Config::default());
    }
}
