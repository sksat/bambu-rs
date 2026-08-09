//! DOOM behind the printer's own LAN interface — the I/O half.
//!
//! The relay already has both halves of what a game needs. It serves a chamber
//! camera on TCP 6000 ([`super::camerad`]) that shows whatever frames it is
//! given, and it sees every control command a client sends before deciding what
//! to do with it ([`super::emulate`]). Point one at the other and Bambu Studio's
//! liveview is a screen and its movement panel is a gamepad.
//!
//! What is here is the process on the other end of that: an engine spawned with
//! a pipe each way. Frames come back on its stdout in the printer's own camera
//! framing — a 16-byte header then a JPEG, exactly what
//! [`crate::core::camerad::frame_header`] describes — so a frame reaches a
//! client's liveview without being re-encoded or even re-framed. Key presses go
//! out on its stdin, two bytes each.
//!
//! The engine is not part of this crate and is not built by it: see
//! `tools/doom/`, which fetches doomgeneric and compiles the small platform
//! layer that speaks this protocol. Keeping DOOM out here is deliberate — a C
//! toolchain in the crate's build would be a cost paid by everyone who ever
//! installs `bambu` for its actual job.
//!
//! **This only ever runs against the synthetic printer.** The safety argument
//! is in two parts: [`crate::core::emulate::ControlPolicy::Intercept`] makes it
//! impossible for an intercepted command to also be forwarded, and
//! [`crate::server::serve`] refuses `--emulate-doom` unless the printer behind
//! the relay is `--fake`. A jog that plays DOOM cannot reach a machine, and a
//! machine cannot be behind a relay that plays DOOM.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::core::camerad::{self, FRAME_HEADER};
use crate::core::doom::{Hold, KeyPad, holds_for};
use crate::server::camerad::FrameFeed;
use crate::server::emulate::ControlSink;

/// A running DOOM: the frames it draws and the keys it is holding.
pub struct DoomEngine {
    frames: FrameFeed,
    /// Presses on their way to the keyboard thread, which owns the deadlines.
    /// Unbounded and non-blocking, because [`ControlSink::consume`] is called
    /// from a client's connection task.
    keys: Sender<Hold>,
}

impl DoomEngine {
    /// Start `program` with `args` and wire both pipes up.
    ///
    /// Returns as soon as the process is running — frames appear in the feed
    /// when the engine has drawn one, which for DOOM means after it has read
    /// its WAD. An engine that fails to start says so on stderr, which is
    /// inherited rather than captured for exactly that reason.
    pub fn spawn(program: &Path, args: &[String]) -> anyhow::Result<Arc<Self>> {
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Inherited: the engine's own log is the only account of what it
            // did with a WAD path, and swallowing it would leave "no picture"
            // as the entire diagnosis.
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow::anyhow!("starting the DOOM engine {}: {e}", program.display()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("the DOOM engine has no stdin to press keys on"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("the DOOM engine has no stdout to draw on"))?;

        let (tx, frames) = tokio::sync::watch::channel(None);
        std::thread::spawn(move || {
            if let Err(e) = pump_frames(stdout, &tx) {
                eprintln!("emulate-doom: the frame stream ended: {e}");
            }
        });

        let (keys, presses) = channel();
        std::thread::spawn(move || keyboard(stdin, &presses));

        // Reaped, and loudly: an engine that dies leaves the liveview frozen on
        // its last frame and every button still ACKing success, which looks
        // from a client exactly like a game nobody is playing.
        std::thread::spawn(move || match child.wait() {
            Ok(status) => eprintln!(
                "emulate-doom: the engine exited ({status}); the liveview will hold its last \
                 frame and the controls will do nothing"
            ),
            Err(e) => eprintln!("emulate-doom: cannot wait on the engine: {e}"),
        });

        Ok(Arc::new(Self { frames, keys }))
    }

    /// The frames it is drawing, for the camera relay to serve.
    pub fn frames(&self) -> FrameFeed {
        self.frames.clone()
    }
}

impl ControlSink for DoomEngine {
    fn consume(&self, payload: &Value) {
        let holds = holds_for(payload);
        // Logged either way. The mapping is the part of this demo most likely
        // to be silently wrong — a button that produces no key looks exactly
        // like a button that never arrived — and this line is what tells them
        // apart.
        match holds.as_slice() {
            [] => eprintln!("emulate-doom: {} — nothing bound", describe(payload)),
            holds => {
                let pressed: Vec<String> = holds
                    .iter()
                    .map(|h| format!("{} for {}ms", h.key.name(), h.ms))
                    .collect();
                eprintln!(
                    "emulate-doom: {} — {}",
                    describe(payload),
                    pressed.join(", ")
                );
            }
        }
        for hold in holds {
            // Err means the engine is gone; its exit was already reported, and
            // saying so again on every press would bury it.
            let _ = self.keys.send(hold);
        }
    }
}

/// A one-line account of a request, for the log: `print.gcode_line "G28"`.
fn describe(payload: &Value) -> String {
    let Some(category) = payload.as_object().and_then(|o| o.keys().next()) else {
        return "an empty request".to_string();
    };
    let command = payload
        .pointer(&format!("/{category}/command"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    match payload
        .pointer(&format!("/{category}/param"))
        .and_then(Value::as_str)
    {
        // Escaped, because a jog's param is several lines and a log line that
        // is several lines is three log lines.
        Some(param) => format!("{category}.{command} {:?}", param),
        None => format!("{category}.{command}"),
    }
}

/// Read frames off the engine's stdout until it stops.
///
/// The framing is the printer's own, so this is the same header
/// [`crate::server::camerad`] writes on the way out — which means a frame is
/// validated once here and then passed through untouched.
fn pump_frames<R: Read>(
    mut reader: R,
    tx: &tokio::sync::watch::Sender<Option<Arc<Vec<u8>>>>,
) -> anyhow::Result<()> {
    let mut header = [0u8; FRAME_HEADER];
    let mut frames = 0u64;
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let len = camerad::frame_len(&header)?;
        let mut frame = vec![0u8; len];
        reader.read_exact(&mut frame)?;
        // The relay's own client refuses anything that is not a whole JPEG, so
        // a frame that would be refused at the far end is dropped here, where
        // the reason can be said out loud.
        if !camerad::is_jpeg(&frame) {
            eprintln!("emulate-doom: the engine sent {len} bytes that are not a JPEG; skipped");
            continue;
        }
        if frames == 0 {
            // The line that separates "the engine never drew anything" from
            // "the client never asked for it".
            eprintln!("emulate-doom: first frame from the engine, {len} bytes");
        }
        frames += 1;
        let _ = tx.send(Some(Arc::new(frame)));
    }
}

/// Hold keys down for as long as they were asked for, and let go on time.
///
/// One thread, one clock, and [`KeyPad`] for the decisions — so the part with
/// the reasoning in it stays testable without a process or a wait, and this is
/// only the loop that turns a deadline into a `write`.
fn keyboard<W: Write>(mut out: W, presses: &Receiver<Hold>) {
    let start = Instant::now();
    let mut pad = KeyPad::new();
    let now = |start: &Instant| start.elapsed().as_millis() as u64;
    loop {
        let events = match pad.next_deadline() {
            // Something is down: wake for the release, or sooner if a press
            // arrives.
            Some(at) => {
                let wait = Duration::from_millis(at.saturating_sub(now(&start)));
                match presses.recv_timeout(wait) {
                    Ok(hold) => pad.press(now(&start), hold),
                    Err(RecvTimeoutError::Timeout) => pad.expire(now(&start)),
                    Err(RecvTimeoutError::Disconnected) => pad.release_all(),
                }
            }
            // Nothing is down, so there is nothing to be late for.
            None => match presses.recv() {
                Ok(hold) => pad.press(now(&start), hold),
                Err(_) => return,
            },
        };
        for event in events {
            if out.write_all(&event.to_wire()).is_err() || out.flush().is_err() {
                eprintln!("emulate-doom: the engine stopped listening; controls are dead");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::doom::Key;
    use serde_json::json;
    use std::sync::Mutex;

    /// A writer a test can read back, standing in for the engine's stdin.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<u8>>>);

    impl Recorder {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn jpeg(fill: u8, len: usize) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.extend(std::iter::repeat_n(fill, len));
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    fn framed(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in frames {
            out.extend_from_slice(&camerad::frame_header(f.len() as u32));
            out.extend_from_slice(f);
        }
        out
    }

    #[tokio::test]
    async fn the_engines_frames_come_out_whole() {
        let want = vec![jpeg(0x11, 2000), jpeg(0x22, 3000)];
        let (tx, feed) = tokio::sync::watch::channel(None);
        pump_frames(std::io::Cursor::new(framed(&want)), &tx).unwrap();
        // The feed keeps only the newest, which is what a viewer joining late
        // should see.
        assert_eq!(feed.borrow().clone().unwrap().as_ref(), &want[1]);
    }

    #[tokio::test]
    async fn a_frame_split_across_reads_is_waited_for() {
        // A pipe hands over whatever it has; a frame arriving in pieces is the
        // normal case, not the exception.
        struct Dribble(Vec<u8>, usize);
        impl Read for Dribble {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.1 >= self.0.len() {
                    return Ok(0);
                }
                buf[0] = self.0[self.1];
                self.1 += 1;
                Ok(1)
            }
        }
        let want = jpeg(0x33, 1500);
        let (tx, feed) = tokio::sync::watch::channel(None);
        pump_frames(Dribble(framed(std::slice::from_ref(&want)), 0), &tx).unwrap();
        assert_eq!(feed.borrow().clone().unwrap().as_ref(), &want);
    }

    #[tokio::test]
    async fn something_that_is_not_a_picture_is_dropped_rather_than_shown() {
        // The engine could be anything the operator named on the command line.
        // A wall of text framed as a photograph is worse than no picture.
        let junk = b"<html>not a doom</html>".repeat(60);
        let good = jpeg(0x44, 1200);
        let (tx, feed) = tokio::sync::watch::channel(None);
        pump_frames(std::io::Cursor::new(framed(&[junk, good.clone()])), &tx).unwrap();
        assert_eq!(feed.borrow().clone().unwrap().as_ref(), &good);
    }

    #[test]
    fn a_press_and_its_release_both_reach_the_engine() {
        let out = Recorder::default();
        let (tx, rx) = channel();
        let writer = out.clone();
        let thread = std::thread::spawn(move || keyboard(writer, &rx));
        tx.send(Hold {
            key: Key::Forward,
            ms: 30,
        })
        .unwrap();
        drop(tx);
        thread.join().unwrap();
        // Down, then up: a key that is never released walks the player into a
        // wall until the process ends.
        assert_eq!(
            out.bytes(),
            vec![1, Key::Forward.code(), 0, Key::Forward.code()]
        );
    }

    #[test]
    fn everything_is_released_when_the_relay_goes_away() {
        // The sender being dropped is `serve` shutting down. A key left down is
        // the state the next engine would inherit.
        let out = Recorder::default();
        let (tx, rx) = channel();
        let writer = out.clone();
        let thread = std::thread::spawn(move || keyboard(writer, &rx));
        tx.send(Hold {
            key: Key::Fire,
            ms: 60_000,
        })
        .unwrap();
        // Give the loop a moment to take the press before hanging up.
        std::thread::sleep(Duration::from_millis(50));
        drop(tx);
        thread.join().unwrap();
        assert_eq!(
            out.bytes(),
            vec![1, Key::Fire.code(), 0, Key::Fire.code()],
            "the press, then the release the shutdown owes it"
        );
    }

    #[test]
    fn a_command_becomes_a_key_and_a_command_with_no_button_becomes_nothing() {
        let (keys, rx) = channel();
        let engine = DoomEngine {
            frames: tokio::sync::watch::channel(None).1,
            keys,
        };
        engine.consume(&json!({"print": {
            "command": "gcode_line", "param": "G91\nG1 Y10 F3000\nG90",
        }}));
        engine.consume(&json!({"print": {"command": "project_file", "url": "ftp:///x"}}));
        let held: Vec<Hold> = rx.try_iter().collect();
        assert_eq!(held.len(), 1, "only the jog is a button: {held:?}");
        assert_eq!(held[0].key, Key::Forward);
    }

    #[test]
    fn a_request_is_described_in_one_line() {
        // A jog's param is three lines of G-code, and a log line that is three
        // lines is three log lines.
        let described = describe(&json!({"print": {
            "command": "gcode_line", "param": "G91\nG1 Y10\nG90",
        }}));
        assert!(!described.contains('\n'), "{described}");
        assert!(described.contains("print.gcode_line"), "{described}");
        assert_eq!(
            describe(&json!({"system": {"command": "ledctrl"}})),
            "system.ledctrl"
        );
    }

    /// The end-to-end one: a real child process, both pipes, no DOOM.
    ///
    /// Everything above drives the halves separately. This proves [`spawn`]
    /// actually joins them to a process — that frames written by something else
    /// arrive in the feed and that a press reaches its stdin — which is the
    /// part a stub reader cannot show.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_real_child_process_is_wired_up_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let keys_seen = dir.path().join("keys");
        // A "DOOM" that draws one frame and writes down what it is told. The
        // frame is 1200 bytes of 0x41 between the JPEG markers, which is enough
        // to pass the relay's own size floor.
        let script = format!(
            r#"printf '\xb4\x04\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00'
printf '\xff\xd8'
for i in $(seq 1 1200); do printf 'A'; done
printf '\xff\xd9'
cat > {}
"#,
            keys_seen.display()
        );
        let engine = DoomEngine::spawn(Path::new("/bin/sh"), &["-c".to_string(), script])
            .expect("the stub engine should start");

        let mut feed = engine.frames();
        let frame = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(f) = feed.borrow_and_update().clone() {
                    return f;
                }
                feed.changed().await.unwrap();
            }
        })
        .await
        .expect("a frame should reach the feed");
        assert!(camerad::is_jpeg(&frame), "{} bytes", frame.len());

        engine.consume(&json!({"print": {"command": "gcode_line", "param": "G28"}}));
        // The press goes out at once; the release follows when the hold runs
        // out, and the stub only writes the file when its stdin closes.
        drop(engine);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(seen) = std::fs::read(&keys_seen)
                && seen.len() >= 4
            {
                assert_eq!(
                    seen,
                    vec![1, Key::Fire.code(), 0, Key::Fire.code()],
                    "the home button should have pressed and released fire"
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the key never reached the engine"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
