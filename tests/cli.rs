//! Black-box CLI tests (device-independent parts): help, config management,
//! and exit codes. The `status` command needs a real printer and is covered by
//! the manual e2e flow, not here.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};

fn tmp_cfg(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bambu-cli-it-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A `bambu` command with an isolated config dir and no `BAMBU_*` env leaking in.
fn bambu(cfg: &Path) -> Command {
    let mut c = Command::cargo_bin("bambu").unwrap();
    c.env("XDG_CONFIG_HOME", cfg)
        .env_remove("BAMBU_IP")
        .env_remove("BAMBU_SERIAL")
        .env_remove("BAMBU_ACCESS_CODE")
        .env_remove("BAMBU_MODEL");
    c
}

#[test]
fn help_succeeds() {
    let cfg = tmp_cfg("help");
    bambu(&cfg)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Bambu Lab"));
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn status_without_config_or_env_is_validation_error() {
    let cfg = tmp_cfg("status-noconf");
    bambu(&cfg).arg("status").assert().code(3); // VALIDATION
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn gcode_without_confirm_is_refused() {
    let cfg = tmp_cfg("gcode-noconfirm");
    bambu(&cfg).args(["gcode", "G28"]).assert().code(4); // CONFIRM_REQUIRED
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn file_ls_without_config_is_validation_error() {
    let cfg = tmp_cfg("file-noconf");
    bambu(&cfg).args(["file", "ls"]).assert().code(3); // VALIDATION (no target)
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn config_add_requires_a_printer_name() {
    let cfg = tmp_cfg("add-noname");
    bambu(&cfg)
        .args([
            "config",
            "add",
            "--ip",
            "203.0.113.4",
            "--serial",
            "S",
            "--access-code",
            "C",
            "--model",
            "a1mini",
        ])
        .assert()
        .code(3);
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn config_add_list_show_roundtrip_redacts_access_code() {
    let cfg = tmp_cfg("roundtrip");
    bambu(&cfg)
        .args([
            "--printer",
            "a1",
            "config",
            "add",
            "--ip",
            "192.0.2.9",
            "--serial",
            "0309TEST",
            "--access-code",
            "12345678",
            "--model",
            "a1mini",
            "--set-default",
        ])
        .assert()
        .success();

    bambu(&cfg)
        .args(["config", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("a1 (default)"));

    bambu(&cfg)
        .args(["config", "show", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("<redacted>"))
        .stdout(predicate::str::contains("12345678").not()); // secret never printed

    let _ = std::fs::remove_dir_all(&cfg);
}
