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
    /// when the movement controls are the point — see `serve --emulate-doom`.
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
        // The live machine answers a request, so its reply carries these; the
        // capture was taken from that answer minus them.
        obj.insert("result".into(), Value::from("success"));
        obj.insert("reason".into(), Value::from("success"));
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

/// Where to read a capture of a real printer's report, if one was configured.
///
/// Set by `--fake-report`; read here rather than threaded through because the
/// synthetic printer is built in one place and this is the only thing it needs
/// from the outside.
pub static CAPTURED_REPORT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The configured capture's `print` object, or `None` if there is no usable one.
///
/// A capture that cannot be read or parsed is a hard error rather than a quiet
/// fall back to the fixture: somebody who passed the flag wants that machine's
/// report, and silently serving a different one would be the sort of thing that
/// takes an evening to notice.
fn captured_report() -> Option<serde_json::Map<String, Value>> {
    let path = CAPTURED_REPORT.get()?;
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading the capture at {}: {e}", path.display()));
    let value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("the capture at {} is not JSON: {e}", path.display()));
    // Accept both the raw report and the `{_meta, message}` envelope the
    // capture tools write, because both are what people have on disk.
    let print = value
        .get("print")
        .or_else(|| value.pointer("/message/print"))
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!(
                "the capture at {} has no `print` object; is it a pushall?",
                path.display()
            )
        });
    Some(print.clone())
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
        // Everything else gets the ACK shape observed on the A1: the echoed
        // sequence id, `result`, `reason`, and no `command` key.
        let Some(seq) = payload
            .pointer(&format!("/{category}/sequence_id"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let _ = self.reports.send(json!({
            category: {"sequence_id": seq, "result": "success", "reason": "success"}
        }));
    }
}

#[cfg(test)]
mod tests {
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
