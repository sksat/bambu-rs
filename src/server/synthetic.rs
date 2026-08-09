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

/// `home_flag` as a real A1 mini reports it with every axis homed.
///
/// **[observed]**, not decoded — see `docs/protocol.md`, where the bit layout
/// is still an open question. Copied whole because a value assembled from
/// guesses about the bits would be a different kind of wrong.
const HOMED: u32 = 847_201_680;

impl SyntheticPrinter {
    /// Start reporting a print in progress. A snapshot goes out immediately —
    /// the relay's cache is seeded from it, exactly as a real connection's
    /// opening `pushall` would — and a delta follows every `interval`.
    pub fn start(interval: Duration) -> Arc<Self> {
        Self::spawn(interval, Job::Printing)
    }

    /// The same printer, idle: nothing running, and the deltas are the small
    /// temperature drift a real machine reports while it sits there.
    ///
    /// It has to keep talking. The relay gives up on a printer that says
    /// nothing for half a minute and disconnects its clients — correctly, since
    /// on a real link that means the machine is gone — so an idle printer that
    /// went quiet would take the whole demo down with it.
    pub fn idle(interval: Duration) -> Arc<Self> {
        Self::spawn(interval, Job::Idle)
    }

    fn spawn(interval: Duration, job: Job) -> Arc<Self> {
        let (reports, _) = broadcast::channel(DEPTH);
        let me = Arc::new(Self {
            reports,
            job,
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
            let _ = ticker.reports.send(ticker.snapshot());
            let mut tick = tokio::time::interval(interval);
            tick.tick().await; // the immediate one; we just sent the snapshot
            loop {
                tick.tick().await;
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
        json!({"print": {
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
            "chamber_temper": 5,
            "cooling_fan_speed": if idle { "0" } else { "100" },
            "spd_lvl": 2,
            "stg_cur": 0,
            // What a real A1 mini reports once its axes are homed, copied from
            // `tests/fixtures/pushall-n1-idle.json` rather than derived: the
            // bit layout is one of the open unknowns in docs/protocol.md.
            //
            // Zero here meant "not homed", and Bambu Studio answers a jog with
            // "Please home all axes" and refuses to move — which in DOOM mode
            // means the movement panel cannot play the game at all. A synthetic
            // printer that has never homed is not a useful stand-in for one.
            "home_flag": HOMED,
            "hms": [],
            "lights_report": [{"node": "chamber_light", "mode": "on"}],
            // The shape a real A1 mini sends, `mode_bits` and `tutk_server`
            // included. They looked like noise until a client was watched
            // deciding whether to open a liveview: with them absent, Bambu
            // Studio does not even try, however loudly `ipcam_dev` says a
            // camera is there. A synthetic printer missing them is not a
            // stand-in a camera client can be tested against.
            "ipcam": {
                "ipcam_dev": "0",
                "ipcam_record": "enable",
                "mode_bits": 3,
                "resolution": "1080p",
                "timelapse": "disable",
                "tutk_server": "disable",
            },
        }})
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
    use super::*;

    #[tokio::test]
    async fn it_seeds_with_a_full_snapshot_then_sends_deltas() {
        let printer = SyntheticPrinter::start(Duration::from_millis(20));
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
        let printer = SyntheticPrinter::start(Duration::from_secs(3600));
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
