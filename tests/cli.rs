//! Black-box CLI tests (device-independent parts): help, config management,
//! and exit codes. The `status` command needs a real printer and is covered by
//! the manual e2e flow, not here.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};

fn tmp_cfg(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bambu-cli-it-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `bambu` command with an isolated config dir and no `BAMBU_*` env leaking in.
/// Runs in the (empty) config dir as CWD so a developer's `.env` in the repo
/// root can't bleed into these hermetic tests.
fn bambu(cfg: &Path) -> Command {
    let mut c = Command::cargo_bin("bambu").unwrap();
    c.current_dir(cfg)
        .env("XDG_CONFIG_HOME", cfg)
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
fn unknown_explicit_printer_errors_instead_of_falling_back_to_env() {
    // A --printer typo must NOT silently run against a BAMBU_*-supplied target.
    let cfg = tmp_cfg("bad-printer");
    Command::cargo_bin("bambu")
        .unwrap()
        .current_dir(&cfg)
        .env("XDG_CONFIG_HOME", &cfg)
        .env("BAMBU_IP", "203.0.113.4")
        .env("BAMBU_SERIAL", "S")
        .env("BAMBU_ACCESS_CODE", "C")
        .env("BAMBU_MODEL", "a1mini")
        .args(["--printer", "nope", "status"])
        .assert()
        .code(3); // UnknownProfile, before any connection attempt
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn job_start_dry_run_shows_payload_without_a_target() {
    let cfg = tmp_cfg("job-dry");
    // dry-run builds the payload only — no config/connection needed.
    bambu(&cfg)
        .args(["job", "start", "/cache/x.gcode.3mf", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("project_file"))
        .stdout(predicate::str::contains("Metadata/plate_1.gcode"));
    bambu(&cfg)
        .args(["job", "start", "/cache/x.gcode", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gcode_file"));
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn expect_guards_reject_raw_gcode_offline() {
    // --expect-md5/--expect-plate are .3mf-only; on a raw .gcode they fail fast
    // (exit 3) before any network I/O, so this needs no printer.
    let cfg = tmp_cfg("expect-gcode");
    bambu(&cfg)
        .args(["job", "start", "/cache/x.gcode", "--expect-plate", "1"])
        .assert()
        .code(3);
    bambu(&cfg)
        .args(["job", "start", "/cache/x.gcode", "--expect-md5", "deadbeef"])
        .assert()
        .code(3);
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn ams_map_out_of_range_fails_fast_offline() {
    // Tray range is checked before any network I/O, so a bad --ams-map exits 3
    // even with no printer. (Length-vs-filament-count needs the 3mf, so it isn't
    // checked here.)
    let cfg = tmp_cfg("ams-range");
    bambu(&cfg)
        .args([
            "job",
            "start",
            "/cache/x.gcode.3mf",
            "--ams-map",
            "0,9",
            "--dry-run",
        ])
        .assert()
        .code(3);
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn job_start_and_pause_need_confirm() {
    let cfg = tmp_cfg("job-confirm");
    bambu(&cfg)
        .args(["job", "start", "/cache/x.gcode"])
        .assert()
        .code(4);
    bambu(&cfg).args(["job", "pause"]).assert().code(4);
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
        .args(["config", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"default\""))
        .stdout(predicate::str::contains("a1"));

    bambu(&cfg)
        .args(["config", "show", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("<redacted>"))
        .stdout(predicate::str::contains("12345678").not()); // secret never printed

    let _ = std::fs::remove_dir_all(&cfg);
}
