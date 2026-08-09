//! A printer that isn't there — the upstream for `serve --fake --emulate`.
//!
//! The relay's own tests drive [`Emulator`](super::emulate::Emulator) in-process,
//! which proves the protocol but not the thing the relay exists for: several
//! *separate processes* sharing one printer. That needs a printer, and the only
//! one available is a real machine.
//!
//! So this is one. It speaks the report side of the LAN protocol — a full
//! `push_status` snapshot, then deltas as a print advances — and answers
//! commands with the ACK the real thing would send. Point a relay at it and the
//! whole stack can be exercised end to end, from `bambu status` down to the
//! wire, with nothing plugged in.
//!
//! Deliberately not a simulation of a printer: no physics, no error injection,
//! no AMS choreography. Just enough truth on the wire that the layers above
//! cannot tell they are being tested.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::emulate::Upstream;
use super::ftpd::PrinterFiles;
use crate::ftp::FileEntry;

/// How many reports a subscriber may fall behind before it misses some.
const DEPTH: usize = 64;

/// A synthetic printer: reports on a timer, ACKs what it is told.
pub struct SyntheticPrinter {
    reports: broadcast::Sender<Value>,
    progress: Mutex<Progress>,
    job: Job,
    /// The serial this printer answers under. A client reads it from the
    /// inventory to recognise the machine.
    serial: String,
}

/// What the synthetic printer is pretending to be doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    /// A print in progress, which is what makes it worth watching.
    Printing,
    /// Sitting there. A client leaves the movement controls enabled for an idle
    /// printer and greys them out for a busy one, so this is the state to be in
    /// when a client's movement controls are the point.
    Idle,
}

#[derive(Debug, Clone, Copy)]
struct Progress {
    layer: u32,
    percent: u32,
    nozzle: f64,
    bed: f64,
}

/// How often to repeat the inventory and a full snapshot, in ticks.
///
/// Small enough that a client connecting late is not left guessing for long,
/// large enough not to be chatter.
const INVENTORY_EVERY: u32 = 5;

/// One real A1 mini's `info.get_version`, as captured from the machine.
///
/// A client asks this before deciding what a printer can do — the `ota`
/// module's `sw_ver` is the firmware the capability registry keys on. A printer
/// that never answers is one of unknown make and firmware, and a client that
/// cannot justify enabling a feature does not enable it.
const REAL_A1_VERSION: &str = include_str!("../../tests/fixtures/get_version-a1mini.json");

/// The captured inventory as the `{"info": …}` a printer sends, wearing
/// `serial` rather than the fixture's scrubbed placeholder.
///
/// Scrubbing the capture was right — it should not carry a machine's serials
/// into the repository — but the placeholder must not reach a client. A client
/// reads the device's serial from the `ota` and `esp32` modules; given the
/// literal string `<redacted>` it cannot match the printer it already knows,
/// treats it as a stranger, and (in Bambu Studio's case) tries to install its
/// application certificate, asking again forever.
///
/// The component serials — mainboard, toolhead, AMS — get the same value. They
/// are not the device's, and inventing plausible-looking ones would be worse:
/// this way anything reading them sees one identity, which is the truth about a
/// printer that is one process.
fn real_a1_version(serial: &str) -> Value {
    static BASE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    let mut info = BASE
        .get_or_init(|| {
            serde_json::from_str::<Value>(REAL_A1_VERSION)
                .ok()
                .and_then(|v| v.get("message")?.get("info").cloned())
                .expect("the captured fixture has message.info")
        })
        .clone();
    if let Some(obj) = info.as_object_mut() {
        // Deliberately *without* `result`/`reason`, though the live machine's
        // reply carries them. `core::report::is_status_report` reads a `result`
        // as "this is an ACK" and the cache then drops the message — so adding
        // them here stopped the relay ever learning the inventory, which is the
        // one thing this function exists to provide. They are added by
        // `version_reply` below, where the message really is an answer.
        if let Some(modules) = obj.get_mut("module").and_then(Value::as_array_mut) {
            for module in modules {
                if let Some(m) = module.as_object_mut() {
                    m.insert("sn".into(), Value::from(serial));
                }
            }
        }
    }
    json!({ "info": info })
}

/// The capture a `--fake-report` was read from, parsed once.
///
/// The parsed report rather than the path it came from, and set before the
/// server starts rather than on first use. Both matter: holding the path meant
/// re-reading the file on every snapshot, so a capture tool rewriting it under
/// a running relay changed the printer's shape mid-conversation, and the
/// failure — a bad file — surfaced as a panic inside a detached ticker task,
/// which unwinds that one task and leaves the listeners up serving nothing.
pub static CAPTURED_REPORT: std::sync::OnceLock<serde_json::Map<String, Value>> =
    std::sync::OnceLock::new();

/// Read and parse a capture, so a bad one is refused while there is still a
/// caller to tell.
///
/// A capture that cannot be read or parsed is a hard error rather than a quiet
/// fall back to the fixture: somebody who passed the flag wants that machine's
/// report, and silently serving a different one would be the sort of thing that
/// takes an evening to notice.
pub fn load_capture(path: &std::path::Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("reading the capture at {}: {e}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("the capture at {} is not JSON: {e}", path.display()))?;
    // Accept both the raw report and the `{_meta, message}` envelope the
    // capture tools write, because both are what people have on disk.
    let print = value
        .get("print")
        .or_else(|| value.pointer("/message/print"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            format!(
                "the capture at {} has no `print` object; is it a pushall?",
                path.display()
            )
        })?;
    let _ = CAPTURED_REPORT.set(print.clone());
    Ok(())
}

/// The configured capture's `print` object, or `None` if there is no usable one.
fn captured_report() -> Option<serde_json::Map<String, Value>> {
    CAPTURED_REPORT.get().cloned()
}

/// The inventory as an *answer*: the modules, plus the result fields and the
/// echoed sequence id a request's reply carries.
fn version_reply(serial: &str, sequence_id: Option<&str>) -> Value {
    let mut message = real_a1_version(serial);
    if let Some(info) = message.get_mut("info").and_then(Value::as_object_mut) {
        if let Some(seq) = sequence_id {
            info.insert("sequence_id".into(), Value::from(seq));
        }
        info.insert("result".into(), Value::from("success"));
        info.insert("reason".into(), Value::from("success"));
    }
    message
}

/// The `sequence_id` of whichever category a request came in under.
fn seq_of(payload: &Value) -> Option<&str> {
    let category = payload.as_object()?.keys().next()?;
    payload
        .pointer(&format!("/{category}/sequence_id"))
        .and_then(Value::as_str)
}

/// One real A1 mini's idle `pushall`, as captured from the machine.
///
/// The synthetic printer is built on this rather than on a hand-written object
/// that looks about right. Sixty-four fields against the twenty that were here
/// before, and the difference is not cosmetic: Bambu Studio decides whether to
/// show filament, and whether the camera button can even be pressed, from
/// fields that were simply missing. A stand-in for a printer has to be the
/// shape of one.
///
/// Already scrubbed — it carries no serial, no access code, and its network
/// section is redacted, which is why the overlay puts a placeholder back.
const REAL_A1_IDLE: &str = include_str!("../../tests/fixtures/pushall-n1-idle.json");

/// The captured report's `print` object, parsed once.
fn real_a1_idle() -> serde_json::Map<String, Value> {
    static BASE: std::sync::OnceLock<serde_json::Map<String, Value>> = std::sync::OnceLock::new();
    // A capture of a real machine, if one was given. The bundled fixture is one
    // A1 mini on one day; a client deciding what a printer can do may care
    // about a field that machine did not have, or a value it happened to hold.
    // Pointing the stand-in at your own capture is the difference between
    // testing against a printer and testing against this printer.
    //
    // Bambu Studio is the reason this exists: with the bundled fixture it kept
    // demanding `security.app_cert_install` and would not proceed, and with a
    // capture taken from the machine that same evening it was satisfied. Which
    // field it reads has not been established — see docs/protocol.md.
    if let Some(obj) = captured_report() {
        return obj;
    }
    BASE.get_or_init(|| {
        serde_json::from_str::<Value>(REAL_A1_IDLE)
            .ok()
            .and_then(|v| v.get("message")?.get("print")?.as_object().cloned())
            .expect("the captured fixture has message.print")
    })
    .clone()
}

/// `home_flag` with X, Y and Z reported as homed.
///
/// The low three bits are the axes. Deduced from two captures of the same
/// machine rather than from documentation, which lists the layout as unknown:
///
/// ```text
/// idle since power-on   847201680   …1001 0000
/// paused mid-print      847201687   …1001 0111
///                                          ^^^ X, Y, Z
/// ```
///
/// A printer that has been idle since boot has *not* homed, which is the trap:
/// the idle capture looks like a fine value to copy and is exactly the one that
/// makes Bambu Studio answer every jog with "Please home all axes". The rest of
/// the word is left as the machine sent it — those bits are still unknown, and
/// inventing them would be the same mistake one level down.
const HOMED: u32 = 847_201_687;

impl SyntheticPrinter {
    /// Start reporting a print in progress. A snapshot goes out immediately —
    /// the relay's cache is seeded from it, exactly as a real connection's
    /// opening `pushall` would — and a delta follows every `interval`.
    pub fn start(serial: &str, interval: Duration) -> Arc<Self> {
        Self::spawn(serial, interval, Job::Printing)
    }

    /// The same printer, idle: nothing running, and the deltas are the small
    /// temperature drift a real machine reports while it sits there.
    ///
    /// It has to keep talking. The relay gives up on a printer that says
    /// nothing for half a minute and disconnects its clients — correctly, since
    /// on a real link that means the machine is gone — so an idle printer that
    /// went quiet would take the whole demo down with it.
    pub fn idle(serial: &str, interval: Duration) -> Arc<Self> {
        Self::spawn(serial, interval, Job::Idle)
    }

    fn spawn(serial: &str, interval: Duration, job: Job) -> Arc<Self> {
        let (reports, _) = broadcast::channel(DEPTH);
        let me = Arc::new(Self {
            reports,
            job,
            serial: serial.to_string(),
            progress: Mutex::new(Progress {
                layer: 0,
                percent: 0,
                nozzle: 25.0,
                bed: 25.0,
            }),
        });
        let ticker = Arc::clone(&me);
        tokio::spawn(async move {
            // The seed. Sent before the first tick so a client that connects
            // straight away is not left waiting a whole interval for state.
            //
            // The inventory goes with it: the relay answers a client's
            // `get_version` from its cache, and the cache only has one if the
            // printer has said so. Without it a client sees a printer of no
            // known firmware and switches features off.
            let _ = ticker.reports.send(real_a1_version(&ticker.serial));
            let _ = ticker.reports.send(ticker.snapshot());
            let mut tick = tokio::time::interval(interval);
            tick.tick().await; // the immediate one; we just sent the snapshot
            let mut ticks: u32 = 0;
            loop {
                tick.tick().await;
                ticks += 1;
                // The seed reaches whoever is subscribed when it is sent, and
                // the emulator subscribes a moment after this task starts — so
                // the inventory, sent once, can miss the one reader that needs
                // it, and a client is then told nothing about the firmware for
                // as long as the process lives. Cheap to repeat, and the cache
                // ignores a repeat it already has.
                if ticks.is_multiple_of(INVENTORY_EVERY) {
                    let _ = ticker.reports.send(real_a1_version(&ticker.serial));
                    let _ = ticker.reports.send(ticker.snapshot());
                }
                let delta = ticker.advance();
                // Err only means nobody is listening yet.
                let _ = ticker.reports.send(delta);
            }
        });
        me
    }

    /// The current state as a full `push_status` (`msg: 0`).
    pub fn snapshot(&self) -> Value {
        let p = *self.progress.lock().expect("progress lock poisoned");
        // An idle machine is not a printing one with the numbers zeroed: it has
        // no job at all, and nothing is being held at temperature.
        let idle = self.job == Job::Idle;
        let mut print = real_a1_idle();
        for (k, v) in json!({
            "command": "push_status",
            "msg": 0,
            "sequence_id": "1000",
            "gcode_state": if idle { "IDLE" } else { "RUNNING" },
            "print_error": 0,
            "subtask_name": if idle { "" } else { "synthetic.gcode.3mf" },
            "gcode_file": if idle { "" } else { "synthetic.gcode.3mf" },
            // The base is an *idle* capture, so every field describing a job
            // says there isn't one. Overlaying `gcode_state: RUNNING` on top and
            // stopping there left a printer reporting a print in progress and
            // `print_type: "idle"` with no task in the same breath — two answers
            // to the same question, which is worse than either.
            //
            // A print started over the LAN is `"local"` — the value this crate
            // already models in `core::status` and serves from its own fake.
            // The ids are placeholders: what matters is that a printer running
            // a job has *some* task, not which. There is no capture of this
            // machine mid-print to take real ones from, so nothing else here is
            // guessed at — `mc_print_stage` in particular keeps the captured
            // value rather than a made-up "printing" one.
            "print_type": if idle { "idle" } else { "local" },
            "task_id": if idle { "0" } else { "1" },
            "subtask_id": if idle { "0" } else { "1" },
            "mc_percent": if idle { 0 } else { p.percent },
            "layer_num": if idle { 0 } else { p.layer },
            "total_layer_num": if idle { 0 } else { 100 },
            "mc_remaining_time": if idle { 0 } else { 100 - p.percent },
            "nozzle_temper": p.nozzle,
            "nozzle_target_temper": if idle { 0.0 } else { 220.0 },
            "bed_temper": p.bed,
            "bed_target_temper": if idle { 0.0 } else { 60.0 },
            "cooling_fan_speed": if idle { "0" } else { "100" },
            // The captured machine had not homed; a stand-in that cannot move
            // is no use to a client whose movement panel is the controller.
            "home_flag": HOMED,
            // The capture's network section is redacted, and a client reads the
            // address from it to decide where the camera and file transfer
            // live. A placeholder here is enough: the relay rewrites it to
            // whatever address it was told to advertise.
            "net": {"conf": 0, "info": [{"ip": 16_777_343, "mask": 65_535}]},
            // Scrubbed out of the capture, and a placeholder is not a signal
            // strength. Somebody's radio is not being described here; this is a
            // printer sitting next to its access point.
            "wifi_signal": "-50dBm",
        })
        .as_object()
        .expect("the overlay is an object")
        {
            print.insert(k.clone(), v.clone());
        }
        json!({ "print": print })
    }

    /// Advance one layer and describe only what changed — a delta, the way the
    /// A1 sends them.
    ///
    /// An idle printer has no layer to advance, but it still has to speak: the
    /// relay treats half a minute of silence as a printer that has gone away.
    /// So it reports what an idle machine really does — a nozzle drifting
    /// around ambient.
    fn advance(&self) -> Value {
        if self.job == Job::Idle {
            let mut p = self.progress.lock().expect("progress lock poisoned");
            p.nozzle = if p.nozzle >= 26.0 {
                24.0
            } else {
                p.nozzle + 0.5
            };
            return json!({"print": {
                "command": "push_status",
                "msg": 1,
                "sequence_id": "1001",
                "nozzle_temper": p.nozzle,
            }});
        }
        let mut p = self.progress.lock().expect("progress lock poisoned");
        p.layer = (p.layer + 1) % 101;
        p.percent = p.layer;
        p.nozzle = (p.nozzle + 8.0).min(220.0);
        p.bed = (p.bed + 3.0).min(60.0);
        json!({"print": {
            "command": "push_status",
            "msg": 1,
            "sequence_id": "1001",
            "mc_percent": p.percent,
            "layer_num": p.layer,
            "nozzle_temper": p.nozzle,
            "bed_temper": p.bed,
        }})
    }
}

impl Upstream for SyntheticPrinter {
    fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.reports.subscribe()
    }

    fn send(&self, payload: Value) {
        let Some(category) = payload.as_object().and_then(|o| o.keys().next()).cloned() else {
            return;
        };
        let command = payload
            .pointer(&format!("/{category}/command"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // A pushall is answered with the state, not an ACK — same as the real
        // printer, whose reply carries its own counter rather than an echo.
        if command == "pushall" {
            let _ = self.reports.send(self.snapshot());
            return;
        }
        // So is a get_version: the answer is the inventory. A generic ACK here
        // would tell a client asking before the relay's cache had warmed that
        // its question succeeded and nothing else — no modules, no firmware,
        // and no way to work out what the printer can do.
        if command == "get_version" {
            let _ = self
                .reports
                .send(version_reply(&self.serial, seq_of(&payload)));
            return;
        }
        // Everything else gets the ACK shape observed on the A1: the echoed
        // sequence id, `result`, `reason`, and — for `print` — no `command`.
        //
        // Outside `print` the command is echoed, because a client cannot
        // otherwise tell which of its questions was answered. Bambu Studio
        // sends `security.app_cert_install`, and given a bare success it asks
        // again, and again, sitting at "Retrieving printer information". The
        // missing `command` is observed for `print` alone; generalising it was
        // an assumption, and this is what it cost.
        let Some(seq) = payload
            .pointer(&format!("/{category}/sequence_id"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let mut ack = serde_json::Map::new();
        ack.insert("sequence_id".into(), Value::from(seq));
        if category != "print" && !command.is_empty() {
            ack.insert("command".into(), Value::from(command.as_str()));
        }
        ack.insert("result".into(), Value::from("success"));
        ack.insert("reason".into(), Value::from("success"));
        let _ = self.reports.send(json!({ category: Value::Object(ack) }));
    }
}

#[cfg(test)]
mod tests {
    /// A printer running a job must not also say it has no job.
    ///
    /// The snapshot is an *idle* capture with the moving values laid over it,
    /// so every field describing a job starts out saying there isn't one.
    /// Overlaying `gcode_state` and stopping there produced a report that read
    /// `RUNNING` and `print_type: "idle"` with task id `0` at the same time —
    /// each field true of the capture, the combination true of no printer.
    #[tokio::test]
    async fn a_printing_snapshot_does_not_also_describe_an_idle_one() {
        let printing =
            super::SyntheticPrinter::start("0309FATEST00001", super::Duration::from_secs(3600))
                .snapshot();
        let p = &printing["print"];
        assert_eq!(p["gcode_state"], "RUNNING");
        assert_eq!(p["print_type"], "local", "a job is running: {p}");
        assert_ne!(p["task_id"], "0", "a job is running but has no task: {p}");
        assert_ne!(p["subtask_id"], "0");

        // And the idle one still describes an idle printer, which is what the
        // capture already said — the overlay must not invent a job either way.
        let idle =
            super::SyntheticPrinter::idle("0309FATEST00001", super::Duration::from_secs(3600))
                .snapshot();
        let i = &idle["print"];
        assert_eq!(i["gcode_state"], "IDLE");
        assert_eq!(i["print_type"], "idle");
        assert_eq!(i["task_id"], "0");
    }

    /// Nothing a client is shown may still be wearing the scrubbing.
    ///
    /// The fixtures are stripped of the machine's identifiers before they enter
    /// the repository, which is right — and twice now a placeholder has gone
    /// out on the wire instead. Bambu Studio read `<redacted>` where the
    /// device's serial belongs, decided it had never met this printer, and
    /// spent the evening trying to pair with it. The next scrubbed field would
    /// do the same thing quietly, so this fails instead.
    #[tokio::test]
    async fn no_scrubbing_placeholder_ever_reaches_a_client() {
        fn placeholders(value: &serde_json::Value, path: &str, found: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    for (k, v) in map {
                        if k.contains("redact") {
                            found.push(format!("{path}.{k}"));
                        }
                        placeholders(v, &format!("{path}.{k}"), found);
                    }
                }
                serde_json::Value::Array(items) => {
                    for (i, v) in items.iter().enumerate() {
                        placeholders(v, &format!("{path}[{i}]"), found);
                    }
                }
                serde_json::Value::String(text) if text.to_lowercase().contains("redact") => {
                    found.push(path.to_string())
                }
                _ => {}
            }
        }

        let printer =
            super::SyntheticPrinter::idle("0309FATEST00001", super::Duration::from_secs(3600));
        let mut found = Vec::new();
        placeholders(&printer.snapshot(), "snapshot", &mut found);
        placeholders(
            &super::real_a1_version("0309FATEST00001"),
            "get_version",
            &mut found,
        );
        assert!(
            found.is_empty(),
            "these would be sent to a client as if they were real: {found:?}"
        );
    }

    use super::*;

    #[tokio::test]
    async fn it_seeds_with_a_full_snapshot_then_sends_deltas() {
        let printer = SyntheticPrinter::start("0309FATEST00001", Duration::from_millis(20));
        let mut rx = printer.subscribe();
        // The seed may already have gone out before we subscribed, so ask for
        // one the way a relay does.
        printer.send(json!({"pushing": {"sequence_id": "0", "command": "pushall"}}));

        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a report should arrive")
            .unwrap();
        assert_eq!(first["print"]["msg"], 0, "the seed is a whole snapshot");
        assert_eq!(first["print"]["gcode_state"], "RUNNING");

        // And it keeps moving, which is what makes it useful to watch.
        let mut saw_delta = false;
        for _ in 0..20 {
            let m = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("more reports")
                .unwrap();
            if m["print"]["msg"] == 1 {
                saw_delta = true;
                break;
            }
        }
        assert!(saw_delta, "a print in progress should emit deltas");
    }

    /// The inventory has to survive the trip through the relay's cache.
    ///
    /// It did not, and nothing said so: adding `result`/`reason` to match the
    /// live machine's *reply* made `is_status_report` read the broadcast as an
    /// ACK, the cache dropped it, and a client was left with `firmware: ?` and
    /// no idea what the printer could do. The relay's own report/ACK rule is
    /// the arbiter here, so the test asks it directly.
    #[tokio::test]
    async fn the_broadcast_inventory_is_a_report_and_the_answer_is_an_answer() {
        use crate::core::report::is_status_report;

        let broadcast = super::real_a1_version("0309FATEST00001");
        assert!(
            is_status_report(&broadcast),
            "the cache drops anything it reads as an ACK, and then no client \
             ever learns the firmware: {broadcast}"
        );
        assert!(broadcast["info"]["result"].is_null());

        // The reply to a request is the other shape: it says which question it
        // answers, and carries the modules a generic ACK would have left out.
        let reply = super::version_reply("0309FATEST00001", Some("77"));
        assert_eq!(reply["info"]["sequence_id"], "77");
        assert_eq!(reply["info"]["result"], "success");
        assert!(
            reply["info"]["module"]
                .as_array()
                .is_some_and(|m| !m.is_empty()),
            "an answer without the modules tells a client nothing"
        );
    }

    /// Outside `print`, the answer says which question it answers.
    #[tokio::test]
    async fn an_ack_outside_print_names_the_command() {
        let printer = SyntheticPrinter::start("0309FATEST00001", Duration::from_secs(3600));
        let mut rx = printer.subscribe();
        printer.send(json!({"security": {"sequence_id": "7", "command": "app_cert_install"}}));

        loop {
            let m = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("an ACK should arrive")
                .unwrap();
            if m["security"]["result"].is_string() {
                assert_eq!(m["security"]["sequence_id"], "7");
                // Without this a client cannot match the answer to its
                // question. Bambu Studio asks forever when it is missing.
                assert_eq!(m["security"]["command"], "app_cert_install");
                break;
            }
        }
    }

    #[tokio::test]
    async fn a_command_is_acked_the_way_the_printer_acks() {
        let printer = SyntheticPrinter::start("0309FATEST00001", Duration::from_secs(3600));
        let mut rx = printer.subscribe();
        printer.send(json!({"print": {"sequence_id": "42", "command": "pause"}}));

        loop {
            let m = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("an ACK should arrive")
                .unwrap();
            if m["print"]["result"].is_string() {
                assert_eq!(m["print"]["sequence_id"], "42", "the id is echoed");
                assert_eq!(m["print"]["result"], "success");
                // The observed ACK carries no `command`; ours must not either,
                // or the relay's report-vs-ACK test double stops matching the
                // real thing.
                assert!(m["print"].get("command").is_none());
                return;
            }
        }
    }
}

/// The synthetic printer's file store: entirely in memory.
///
/// `--fake --emulate` must not point the FTP relay at a real
/// [`LivePrinterFiles`](super::ftpd::LivePrinterFiles). The synthetic target's
/// address is loopback on the default port — which is the relay *itself*, so
/// one FTP operation would open relay connections into the relay until the
/// connection cap stopped it. With a custom port it would instead reach
/// whatever unrelated service happened to be listening.
#[derive(Default)]
pub struct SyntheticFiles {
    files: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
}

impl SyntheticFiles {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn listing(&self, dir: &str) -> Vec<(String, usize)> {
        let prefix = if dir.ends_with('/') {
            dir.to_string()
        } else {
            format!("{dir}/")
        };
        self.files
            .lock()
            .expect("files lock poisoned")
            .iter()
            .filter_map(|(path, body)| {
                let rest = path.strip_prefix(&prefix)?;
                (!rest.contains('/')).then(|| (rest.to_string(), body.len()))
            })
            .collect()
    }
}

impl PrinterFiles for SyntheticFiles {
    fn list_raw(&self, dir: &str) -> Result<Vec<String>, String> {
        Ok(self
            .listing(dir)
            .into_iter()
            .map(|(name, size)| format!("-rw-rw-rw-   1 root  root  {size:>8} Jan  1 00:00 {name}"))
            .collect())
    }
    fn list_names(&self, dir: &str) -> Result<Vec<String>, String> {
        Ok(self.listing(dir).into_iter().map(|(n, _)| n).collect())
    }
    fn entries(&self, dir: &str) -> Result<Vec<FileEntry>, String> {
        Ok(self
            .listing(dir)
            .into_iter()
            .map(|(name, size)| FileEntry {
                name,
                is_dir: false,
                size: size as u64,
            })
            .collect())
    }
    fn upload(&self, local: &std::path::Path, remote: &str) -> Result<u64, String> {
        let body = std::fs::read(local).map_err(|e| e.to_string())?;
        let n = body.len() as u64;
        self.files
            .lock()
            .expect("files lock poisoned")
            .insert(remote.to_string(), body);
        Ok(n)
    }
    fn download(&self, remote: &str, local: &std::path::Path) -> Result<u64, String> {
        let files = self.files.lock().expect("files lock poisoned");
        let body = files
            .get(remote)
            .ok_or_else(|| format!("no such file: {remote}"))?;
        std::fs::write(local, body).map_err(|e| e.to_string())?;
        Ok(body.len() as u64)
    }
    fn delete(&self, remote: &str) -> Result<(), String> {
        self.files
            .lock()
            .expect("files lock poisoned")
            .remove(remote);
        Ok(())
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let mut files = self.files.lock().expect("files lock poisoned");
        let body = files
            .remove(from)
            .ok_or_else(|| format!("no such file: {from}"))?;
        files.insert(to.to_string(), body);
        Ok(())
    }
    fn mkdir(&self, _path: &str) -> Result<(), String> {
        Ok(()) // directories are implied by the paths themselves
    }
    fn rmdir(&self, _path: &str) -> Result<(), String> {
        Ok(())
    }
}
