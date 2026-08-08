//! Black-box CLI tests (device-independent parts): help, config management,
//! and exit codes. The `status` command needs a real printer and is covered by
//! the manual e2e flow, not here.
//!
//! Gated on the `cli` feature: these drive the `bambu` binary, which isn't built
//! without `cli`, so a lib-only build must not try to run them.
#![cfg(feature = "cli")]

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
fn calibrate_flags_dry_run_and_confirm_gate() {
    let cfg = tmp_cfg("calibrate");
    // A specific routine flag; dry run is human-readable (stderr) by default.
    bambu(&cfg)
        .args(["calibrate", "--bed-level", "--dry-run"])
        .assert()
        .success()
        .stderr(predicate::str::contains("bed level"))
        .stderr(predicate::str::contains("vibration").not());
    // No routine flag → defaults to ALL routines (mirrors the dashboard picker).
    bambu(&cfg)
        .args(["calibrate", "--dry-run", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"bed_level\": true"))
        .stdout(predicate::str::contains("\"vibration\": true"))
        .stdout(predicate::str::contains("\"motor_noise\": true"));
    // Not a dry run and no --confirm → refused before any I/O (exit 4).
    bambu(&cfg)
        .args(["calibrate", "--vibration"])
        .assert()
        .code(4); // CONFIRM_REQUIRED
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
    bambu(&cfg).args(["job", "clear-error"]).assert().code(4);
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn upload_rejects_expect_guards() {
    // --expect-md5/--expect-plate don't apply with --upload (the local md5 is used
    // directly); reject up front (exit 3) rather than silently ignore. No file or
    // network needed — it fails before any inspection.
    let cfg = tmp_cfg("upload-expect");
    bambu(&cfg)
        .args([
            "job",
            "start",
            "/tmp/whatever.3mf",
            "--upload",
            "--expect-plate",
            "1",
        ])
        .assert()
        .code(3); // VALIDATION
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn upload_dest_must_keep_the_file_type() {
    // A --dest that flips the .3mf-ness would start the wrong command type for the
    // bytes uploaded; reject it (exit 3) before touching the file or network.
    let cfg = tmp_cfg("upload-dest-type");
    bambu(&cfg)
        .args([
            "job",
            "start",
            "/tmp/model.gcode.3mf",
            "--upload",
            "--dest",
            "/cache/model.gcode",
            "--confirm",
        ])
        .assert()
        .code(3); // VALIDATION
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn upload_missing_local_file_is_validation_error() {
    // `--upload` reads + inspects the LOCAL file before anything else; a missing
    // path is a clean validation error (exit 3), not a transport/confirm error.
    let cfg = tmp_cfg("upload-missing");
    bambu(&cfg)
        .args([
            "job",
            "start",
            "/no/such/file.gcode.3mf",
            "--upload",
            "--confirm",
        ])
        .assert()
        .code(3); // VALIDATION
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn gcode_without_confirm_is_refused() {
    let cfg = tmp_cfg("gcode-noconfirm");
    bambu(&cfg).args(["gcode", "G28"]).assert().code(4); // CONFIRM_REQUIRED
    let _ = std::fs::remove_dir_all(&cfg);
}

/// Write a `.gcode` sequence next to the throwaway config and return its path.
fn seq_file(cfg: &Path, name: &str, body: &str) -> String {
    let p = cfg.join(name);
    std::fs::write(&p, body).unwrap();
    p.to_string_lossy().into_owned()
}

/// A vendor-shaped sequence: comment header, blank lines, trailing `; \n`.
const SEQ: &str = "; plate swap - v05\n\nG90;\nG28;\n\n; park\nG0 X-10; \\n\n";

#[test]
fn gcode_from_file_dry_run_resolves_the_steps_with_source_lines() {
    // --dry-run resolves the file and stops: no config, no printer, no
    // connection — which is also why this is testable without hardware.
    let cfg = tmp_cfg("gcode-dry");
    let path = seq_file(&cfg, "swap.gcode", SEQ);
    bambu(&cfg)
        .args(["gcode", "--from-file", &path, "--dry-run", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"count\": 3"))
        // The source line numbers are the point: 3/4/7, not 1/2/3.
        .stdout(predicate::str::contains("\"line\": 3"))
        .stdout(predicate::str::contains("\"line\": 4"))
        .stdout(predicate::str::contains("\"line\": 7"))
        .stdout(predicate::str::contains("\"gcode\": \"G0 X-10\""));
    // Human-readable by default, JSON only with --json (as `calibrate --dry-run`).
    bambu(&cfg)
        .args(["gcode", "--from-file", &path, "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("[3/3] line 7: G0 X-10"))
        .stderr(predicate::str::contains("nothing sent"));
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn gcode_from_file_missing_is_validation_error() {
    let cfg = tmp_cfg("gcode-missing");
    bambu(&cfg)
        .args(["gcode", "--from-file", "/no/such/swap.gcode", "--dry-run"])
        .assert()
        .code(3); // VALIDATION
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn gcode_from_file_with_nothing_to_send_is_validation_error() {
    // Comments and blanks only: silently sending zero lines would look like the
    // macro ran, so it's an error, not a no-op success.
    let cfg = tmp_cfg("gcode-empty");
    let empty = seq_file(&cfg, "empty.gcode", "");
    let comments = seq_file(
        &cfg,
        "comments.gcode",
        "; header\n\n   \n; and that's all\n",
    );
    for path in [&empty, &comments] {
        bambu(&cfg)
            .args(["gcode", "--from-file", path, "--dry-run"])
            .assert()
            .code(3); // VALIDATION
    }
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn gcode_from_file_unsafe_lines_are_all_named_and_refused() {
    // Vetting happens before the plan is shown, so even --dry-run refuses — and
    // it names EVERY offending line, because the operator confirms the sequence
    // as a unit rather than fixing one line per attempt.
    let cfg = tmp_cfg("gcode-unsafe");
    let path = seq_file(&cfg, "hot.gcode", "M104 S400\nG28\nM109 S500\n");
    bambu(&cfg)
        .args(["gcode", "--from-file", &path, "--dry-run"])
        .assert()
        .code(3) // VALIDATION
        .stderr(predicate::str::contains("line 1: M104 S400"))
        .stderr(predicate::str::contains("line 3: M109 S500"));
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn gcode_takes_a_line_or_a_file_but_not_both_and_not_neither() {
    let cfg = tmp_cfg("gcode-exclusive");
    let path = seq_file(&cfg, "swap.gcode", SEQ);
    // Both: a clap usage error (exit 2), before the file is read.
    bambu(&cfg)
        .args(["gcode", "G28", "--from-file", &path, "--dry-run"])
        .assert()
        .code(2);
    // Neither: nothing to send.
    bambu(&cfg).args(["gcode", "--dry-run"]).assert().code(3); // VALIDATION
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn gcode_from_file_without_confirm_is_refused() {
    // One --confirm covers the whole sequence; without it nothing connects.
    let cfg = tmp_cfg("gcode-seq-noconfirm");
    let path = seq_file(&cfg, "swap.gcode", SEQ);
    bambu(&cfg)
        .args(["gcode", "--from-file", &path])
        .assert()
        .code(4); // CONFIRM_REQUIRED
    let _ = std::fs::remove_dir_all(&cfg);
}

#[cfg(feature = "server")]
#[test]
fn via_serve_unreachable_is_a_transport_error() {
    // --via-serve must NOT silently fall back to a direct MQTT connection when the
    // serve is down: an unreachable serve is a hard transport error (exit 7). Port
    // 1 has no listener, so the connection is refused immediately (no hang). This
    // also exercises the --watch path, whose first poll fails the same way.
    let cfg = tmp_cfg("via-serve-down");
    for extra in [vec![], vec!["--watch"]] {
        let mut args = vec!["status"];
        args.extend(extra);
        args.extend(["--via-serve", "http://127.0.0.1:1"]);
        bambu(&cfg)
            .env_remove("BAMBU_SERVE_URL")
            .args(&args)
            .assert()
            .code(7); // TRANSPORT
    }
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

/// Write a config with one profile that owns a relative-path sequence, and the
/// sequence file it names. Returns the config dir.
fn cfg_with_sequence(tag: &str) -> std::path::PathBuf {
    let cfg = tmp_cfg(tag);
    let dir = cfg.join("bambu-rs");
    std::fs::create_dir_all(dir.join("sequences")).unwrap();
    std::fs::write(dir.join("sequences/swap.gcode"), SEQ).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "default_printer = \"a1\"\n\
         [printers.a1]\n\
         ip = \"192.0.2.10\"\n\
         serial = \"SA\"\n\
         model = \"a1mini\"\n\
         mode = \"lan\"\n\
         access_code = \"00000000\"\n\
         [printers.a1.sequences]\n\
         swap = \"sequences/swap.gcode\"\n",
    )
    .unwrap();
    cfg
}

#[test]
fn gcode_sequence_resolves_through_the_profile_from_any_directory() {
    let cfg = cfg_with_sequence("seq-resolve");
    // The path in the config is RELATIVE. Run from a directory that is not the
    // config dir: anchoring to the cwd would break here, which is the whole
    // reason it anchors to the config dir instead.
    let elsewhere = tmp_cfg("seq-resolve-cwd");
    bambu(&cfg)
        .current_dir(&elsewhere)
        .args(["gcode", "--sequence", "swap", "--dry-run", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"count\": 3"))
        .stdout(predicate::str::contains("sequences/swap.gcode"));
    let _ = std::fs::remove_dir_all(&cfg);
    let _ = std::fs::remove_dir_all(&elsewhere);
}

#[test]
fn gcode_sequence_unknown_name_lists_the_defined_ones() {
    let cfg = cfg_with_sequence("seq-unknown");
    bambu(&cfg)
        .args(["gcode", "--sequence", "swaap", "--dry-run"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("swaap"))
        .stderr(predicate::str::contains("swap"));
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn gcode_sequence_refuses_to_run_on_a_different_machine() {
    // A sequence describes one machine's hardware. Selecting profile a1 while
    // pointing the connection elsewhere would send its plate-changer motion to
    // a printer that has no such accessory.
    let cfg = cfg_with_sequence("seq-identity");
    bambu(&cfg)
        .env("BAMBU_IP", "10.0.0.1")
        .args(["gcode", "--sequence", "swap", "--dry-run"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("must not be sent to another"));
    // Same machine spelled out explicitly is harmless, so it still runs.
    bambu(&cfg)
        .env("BAMBU_IP", "192.0.2.10")
        .args(["gcode", "--sequence", "swap", "--dry-run"])
        .assert()
        .success();
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn gcode_sequence_without_a_profile_says_where_sequences_live() {
    let cfg = tmp_cfg("seq-noprofile"); // no config at all
    bambu(&cfg)
        .args(["gcode", "--sequence", "swap", "--dry-run"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("--from-file"));
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn config_add_refuses_to_overwrite_and_set_edits_in_place() {
    // `add` rebuilds the profile from its flags, so overwriting would destroy
    // everything the flags don't cover — the printer's sequences among them.
    // It refuses; `set` is the way to change one field.
    let cfg = cfg_with_sequence("cfg-set");
    bambu(&cfg)
        .args([
            "--printer",
            "a1",
            "config",
            "add",
            "--ip",
            "192.0.2.99",
            "--serial",
            "SA",
            "--access-code",
            "00000000",
            "--model",
            "a1mini",
        ])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("config set"));

    bambu(&cfg)
        .args(["--printer", "a1", "config", "set", "--ip", "192.0.2.99"])
        .assert()
        .success();
    let saved = std::fs::read_to_string(cfg.join("bambu-rs/config.toml")).unwrap();
    assert!(saved.contains("192.0.2.99"), "the IP should have changed");
    assert!(
        saved.contains("sequences/swap.gcode"),
        "editing one field must leave the rest alone:\n{saved}"
    );

    // --force is the deliberate "replace it whole", and says so by dropping them.
    bambu(&cfg)
        .args([
            "--printer",
            "a1",
            "config",
            "add",
            "--ip",
            "192.0.2.1",
            "--serial",
            "SB",
            "--access-code",
            "11111111",
            "--model",
            "a1mini",
            "--force",
        ])
        .assert()
        .success();
    let replaced = std::fs::read_to_string(cfg.join("bambu-rs/config.toml")).unwrap();
    assert!(
        !replaced.contains("sequences/swap.gcode"),
        "--force replaces"
    );
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn config_set_needs_a_field_and_an_existing_profile() {
    let cfg = cfg_with_sequence("cfg-set-guard");
    bambu(&cfg)
        .args(["--printer", "a1", "config", "set"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("at least one field"));
    bambu(&cfg)
        .args(["--printer", "nope", "config", "set", "--ip", "1.2.3.4"])
        .assert()
        .code(3);
    let _ = std::fs::remove_dir_all(&cfg);
}
