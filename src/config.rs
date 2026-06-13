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
        }
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
