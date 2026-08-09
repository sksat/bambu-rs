//! End-to-end relay test, across real processes, with no printer.
//!
//! The relay's unit tests drive it in-process, which proves the protocol but not
//! the thing it exists for: several **separate** clients sharing one printer
//! connection, over real sockets, through the real binary.
//!
//! So this builds the whole chain out of the binary itself:
//!
//! ```text
//!   bambu serve --fake --emulate    the printer (synthetic, no hardware)
//!            ▲
//!            │  one MQTT connection
//!   bambu serve --emulate           the relay under test
//!            ▲
//!            │  many
//!   bambu status  ×N                ordinary clients (rumqttc)
//! ```
//!
//! Every port is ephemeral, so this runs in parallel with anything else and
//! needs no privileges — which is exactly why the client gained `--mqtt-port`.
#![cfg(all(feature = "cli", feature = "relay"))]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SERIAL: &str = "E2ETESTSERIAL01";
const CODE: &str = "13572468";

/// A child process killed when the test ends, however it ends.
struct Proc {
    child: Child,
    name: &'static str,
    /// Everything the child has said, collected by a reader thread. A pipe
    /// nobody drains eventually blocks the writer, and the counts this test
    /// asserts on live in here.
    stderr: Arc<Mutex<String>>,
}

impl Proc {
    /// What the child has printed to stderr so far.
    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Only interesting when something went wrong; a passing run says nothing.
        if std::thread::panicking() {
            eprintln!("--- {} stderr ---\n{}", self.name, self.stderr());
        }
    }
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bambu"))
}

/// A port nothing is listening on, from below the kernel's ephemeral range.
///
/// `bind(":0")` and drop is the obvious version, and it loses a race that is not
/// theoretical: the kernel hands that very port to the *next* caller asking for
/// an ephemeral one, and this suite holds six of them across two child processes
/// while the rest of the test run is doing the same. It cost a `Address already
/// in use` on a full-suite run.
///
/// Numbers here come from a band the kernel never assigns on its own, so nothing
/// can be given the port between choosing it and the child binding it. A
/// collision now needs another process to have picked the same number
/// deliberately. The offsets keep concurrent test binaries — and concurrent
/// tests inside one binary — out of each other's way.
fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    const BASE: u16 = 20_000;
    const SPAN: u16 = 10_000;
    static NEXT: AtomicU16 = AtomicU16::new(0);

    let mine = NEXT.fetch_add(1, Ordering::Relaxed);
    let start = (std::process::id() as u16).wrapping_add(mine) % SPAN;
    for i in 0..SPAN {
        let port = BASE + (start.wrapping_add(i) % SPAN);
        // Bound and dropped only to ask "is anything here?" — see above for why
        // that answer keeps until the child binds it.
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("no free port in {BASE}..{}", BASE + SPAN);
}

fn spawn(name: &'static str, args: &[&str]) -> Proc {
    let mut child = bin()
        .args(args)
        // A developer's real printer must not leak in and turn this into a
        // test against hardware.
        .env_remove("BAMBU_IP")
        .env_remove("BAMBU_SERIAL")
        .env_remove("BAMBU_ACCESS_CODE")
        .env_remove("BAMBU_MODEL")
        .env_remove("BAMBU_MQTT_PORT")
        .env_remove("BAMBU_FTPS_PORT")
        .env_remove("BAMBU_DETECT_PORT")
        .env(
            "XDG_CONFIG_HOME",
            std::env::temp_dir().join("bambu-e2e-none"),
        )
        .current_dir(std::env::temp_dir()) // away from the repo's own .env
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawning {name}: {e}"));
    let collected = Arc::new(Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let sink = Arc::clone(&collected);
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                sink.lock().unwrap().push_str(&line);
                sink.lock().unwrap().push('\n');
            }
        });
    }
    Proc {
        child,
        name,
        stderr: collected,
    }
}

/// Wait for something to start listening, or fail the test saying what didn't.
fn wait_for_port(port: u16, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{what} never started listening on {port}");
}

/// Send the device-detect probe to `port` and return the decoded reply.
///
/// This is the exchange Bambu Studio's "add printer by IP" performs *before*
/// MQTT — it is the reason a relay can serve 8883 perfectly and still never be
/// found.
fn probe_detect(port: u16) -> serde_json::Value {
    use bambu_rs::core::detect;
    use std::io::{Read, Write};

    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("connecting to the detect port {port}: {e}"));
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request = serde_json::json!({"login": {"command": "detect", "sequence_id": "20004"}});
    sock.write_all(&detect::encode(&request)).unwrap();

    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        if let Some((reply, _)) = detect::decode(&buf).expect("a well-formed reply") {
            return reply;
        }
        let n = sock
            .read(&mut chunk)
            .unwrap_or_else(|e| panic!("reading the detect reply from {port}: {e}"));
        assert_ne!(n, 0, "the detect port closed without answering");
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// `bambu status --json` against a relay on `port`.
fn status_via(port: u16) -> (bool, String) {
    let out = bin()
        .args([
            "--ip",
            "127.0.0.1",
            "--mqtt-port",
            &port.to_string(),
            "--serial",
            SERIAL,
            "--access-code",
            CODE,
            "--model",
            "a1mini",
            "--json",
            "status",
        ])
        .env_remove("BAMBU_IP")
        .env_remove("BAMBU_MQTT_PORT")
        .current_dir(std::env::temp_dir())
        .output()
        .expect("running bambu status");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

#[test]
fn several_client_processes_share_one_printer_through_the_relay() {
    let printer_mqtt = free_port();
    let printer_ftp = free_port();
    let relay_mqtt = free_port();
    let relay_ftp = free_port();
    let printer_http = free_port();
    let relay_http = free_port();
    let printer_detect = free_port();
    let relay_detect = free_port();

    // 1. The printer: synthetic, nothing connected to it.
    let printer = spawn(
        "synthetic printer",
        &[
            "--serial",
            SERIAL,
            "--access-code",
            CODE,
            "serve",
            "--fake",
            "--port",
            &printer_http.to_string(),
            "--emulate",
            "--emulate-host",
            "127.0.0.1",
            "--emulate-port",
            &printer_mqtt.to_string(),
            "--emulate-ftp-port",
            &printer_ftp.to_string(),
            "--emulate-detect-port",
            &printer_detect.to_string(),
            "--emulate-detect-tls-port",
            &free_port().to_string(),
        ],
    );
    wait_for_port(printer_mqtt, "the synthetic printer");

    // 2. The relay under test, pointed at it. This is the real article: a
    //    LivePrinterLink holding one MQTT connection upstream.
    let _relay = spawn(
        "relay",
        &[
            "--ip",
            "127.0.0.1",
            "--mqtt-port",
            &printer_mqtt.to_string(),
            "--ftps-port",
            &printer_ftp.to_string(),
            "--detect-port",
            &printer_detect.to_string(),
            "--serial",
            SERIAL,
            "--access-code",
            CODE,
            "--model",
            "a1mini",
            "serve",
            "--port",
            &relay_http.to_string(),
            "--emulate",
            "--emulate-host",
            "127.0.0.1",
            "--emulate-port",
            &relay_mqtt.to_string(),
            "--emulate-ftp-port",
            &relay_ftp.to_string(),
            "--emulate-detect-port",
            &relay_detect.to_string(),
            "--emulate-detect-tls-port",
            &free_port().to_string(),
        ],
    );
    wait_for_port(relay_mqtt, "the relay");

    // 3. An ordinary client process, reading the printer through the relay.
    //    Retried because the relay's cache is seeded by the upstream's first
    //    report, which may not have arrived the instant the port opened.
    let deadline = Instant::now() + Duration::from_secs(45);
    let ok = loop {
        let (ok, out) = status_via(relay_mqtt);
        if ok && out.contains("\"gcode_state\"") {
            break out;
        }
        assert!(
            Instant::now() < deadline,
            "no status through the relay in time; last output:\n{out}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    assert!(
        ok.contains("RUNNING"),
        "the synthetic printer's state should arrive through the relay:\n{ok}"
    );

    // 4. Several at once — the whole point. Each is its own process with its
    //    own MQTT connection to the relay; upstream there is still only one.
    let handles: Vec<_> = (0..4)
        .map(|_| std::thread::spawn(move || status_via(relay_mqtt)))
        .collect();
    for (i, h) in handles.into_iter().enumerate() {
        let (ok, out) = h.join().expect("client thread");
        assert!(ok, "concurrent client {i} failed:\n{out}");
        assert!(
            out.contains("\"gcode_state\""),
            "concurrent client {i} got no state:\n{out}"
        );
    }

    // 5. The probe Studio makes before it will open MQTT at all. Answering it
    //    is what makes the relay addable by IP; without it the client gives up
    //    silently, having sent nothing a server could log.
    let reply = probe_detect(relay_detect);
    assert_eq!(reply["login"]["sequence_id"], "20004", "the id is echoed");
    assert_eq!(reply["login"]["bind"], "free");
    assert_eq!(reply["login"]["connect"], "lan");
    //    And the identity came *from the printer*, through the relay — not
    //    composed by the relay from what it was configured with. Only the
    //    upstream knows it calls itself "synthetic": a relay that invented an
    //    answer would still satisfy every assertion above.
    assert_eq!(
        reply["login"]["name"], "synthetic",
        "the relay should pass the printer's own identity through:\n{reply}"
    );
    assert_eq!(reply["login"]["id"], SERIAL);

    // 6. The property the whole feature exists for, and the one every
    //    assertion above would still satisfy if it broke: the *printer* saw a
    //    single client throughout. A regression that opened one upstream
    //    connection per downstream client would serve all five of those reads
    //    perfectly well.
    let seen = printer.stderr();
    let connects = seen.matches("connected as").count();
    assert_eq!(
        connects, 1,
        "the printer should have seen exactly one client (the relay), saw {connects}:\n{seen}"
    );
}

#[test]
fn the_relay_refuses_a_client_with_the_wrong_access_code() {
    let printer_mqtt = free_port();
    let relay_mqtt = free_port();

    let _printer = spawn(
        "synthetic printer",
        &[
            "--serial",
            SERIAL,
            "--access-code",
            CODE,
            "serve",
            "--fake",
            "--port",
            &free_port().to_string(),
            "--emulate",
            "--emulate-host",
            "127.0.0.1",
            "--emulate-port",
            &printer_mqtt.to_string(),
            "--emulate-ftp-port",
            &free_port().to_string(),
            "--emulate-detect-port",
            &free_port().to_string(),
            "--emulate-detect-tls-port",
            &free_port().to_string(),
        ],
    );
    wait_for_port(printer_mqtt, "the synthetic printer");

    let _relay = spawn(
        "relay",
        &[
            "--ip",
            "127.0.0.1",
            "--mqtt-port",
            &printer_mqtt.to_string(),
            "--serial",
            SERIAL,
            "--access-code",
            CODE,
            "--model",
            "a1mini",
            "serve",
            "--port",
            &free_port().to_string(),
            "--emulate",
            "--emulate-host",
            "127.0.0.1",
            "--emulate-port",
            &relay_mqtt.to_string(),
            "--emulate-ftp-port",
            &free_port().to_string(),
            "--emulate-detect-port",
            &free_port().to_string(),
            "--emulate-detect-tls-port",
            &free_port().to_string(),
        ],
    );
    wait_for_port(relay_mqtt, "the relay");

    // Prove the relay actually serves first. Without this the test would pass
    // just as well against a relay that was dead, refusing everyone — which is
    // not the property being claimed.
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let (ok, out) = status_via(relay_mqtt);
        if ok && out.contains("\"gcode_state\"") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the relay should serve the right access code:\n{out}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // The access code is the only thing between a LAN peer and the machine.
    let out = bin()
        .args([
            "--ip",
            "127.0.0.1",
            "--mqtt-port",
            &relay_mqtt.to_string(),
            "--serial",
            SERIAL,
            "--access-code",
            "00000000",
            "--model",
            "a1mini",
            "status",
        ])
        .env_remove("BAMBU_IP")
        .env_remove("BAMBU_MQTT_PORT")
        .current_dir(std::env::temp_dir())
        .output()
        .expect("running bambu status");
    assert!(
        !out.status.success(),
        "a wrong access code must not read the printer:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// DOOM through the printer's own LAN interface, across processes.
///
/// The relay's own tests prove the two halves separately: that an intercepted
/// command cannot reach the printer (`server::emulate`) and that the engine's
/// pipes are read and written correctly (`server::doom`). What only a real
/// process can show is the wiring in between — that the flags reach the
/// emulator at all, that the game is served on the port a client's liveview
/// looks at, and that a button pressed by an ordinary client comes out as a
/// keypress at the engine's stdin. The clap bug that read the engine's own
/// `-iwad` as one of our flags got past everything except starting the thing.
///
/// The "engine" here is a shell script, not DOOM: this is a test of the
/// plumbing, and it should not need a WAD or a C compiler.
#[cfg(unix)]
#[test]
fn a_client_plays_a_game_through_the_printers_own_controls() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("bambu-doom-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let keys = dir.join("keys");
    let engine = dir.join("engine.sh");
    // A status record saying the player is on 50 health, then one framed
    // picture, then everything it is told, written down. The `cat` holds stdin
    // open, which is what keeps it alive the way a game would.
    //
    // The status record's length word is zero and its magic is "DOOM"; a reader
    // that mistook it for a frame would hand four bytes of binary to a client's
    // decoder, so this test is also where that would show.
    std::fs::write(
        &engine,
        format!(
            "#!/bin/sh\n\
             printf '\\000\\000\\000\\000DOOM\\004\\000\\000\\000\\000\\000\\000\\000'\n\
             printf '\\062\\000\\000\\000'\n\
             printf '\\264\\004\\000\\000\\000\\000\\000\\000\\001\\000\\000\\000\\000\\000\\000\\000'\n\
             printf '\\377\\330'\n\
             i=0; while [ $i -lt 1200 ]; do printf 'A'; i=$((i+1)); done\n\
             printf '\\377\\331'\n\
             cat > {}\n",
            keys.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mqtt = free_port();
    let camera = free_port();
    let http = free_port();
    let serve = spawn(
        "doom printer",
        &[
            "--serial",
            SERIAL,
            "--access-code",
            CODE,
            "serve",
            "--fake",
            "--port",
            &http.to_string(),
            "--emulate",
            "--emulate-host",
            "127.0.0.1",
            "--emulate-port",
            &mqtt.to_string(),
            "--emulate-no-detect",
            "--emulate-doom",
            "--emulate-doom-engine",
            engine.to_str().unwrap(),
            "--emulate-doom-port",
            &camera.to_string(),
        ],
    );
    wait_for_port(camera, "the game's camera port");

    // 1. What a client's liveview would show is what the engine drew.
    let target = bambu_rs::config::ResolvedTarget {
        camera_port: camera,
        ..bambu_rs::config::ResolvedTarget::new(
            "127.0.0.1",
            SERIAL,
            CODE,
            bambu_rs::core::model::Model::A1Mini,
        )
    };
    let frame = bambu_rs::camera::CameraClient::new(target)
        .with_timeout(Duration::from_secs(20))
        .snapshot()
        .unwrap_or_else(|e| panic!("the game should be served as the chamber camera: {e}"));
    assert!(
        bambu_rs::core::camerad::is_jpeg(&frame),
        "got {} bytes that are not a picture",
        frame.len()
    );

    // 2. The player's health is the nozzle temperature, read by a client that
    //    knows nothing about any of this. 50 health is 122.5 °C on the scale in
    //    `core::doom`, and the target is the full-health value, so a client
    //    drawing current against target is drawing a health bar.
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let (ok, out) = status_via(mqtt);
        if ok && out.contains("122.5") {
            assert!(
                out.contains("\"nozzle_target\": 220.0"),
                "the target should be full health:\n{out}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the game's health never reached the nozzle:\n{out}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // 3. And the home button reaches the game as the trigger. Sent with the
    //    ordinary client, which waits for the ACK — so this also shows a client
    //    is not left hanging on a command that went to a game instead.
    let out = bin()
        .args([
            "--ip",
            "127.0.0.1",
            "--mqtt-port",
            &mqtt.to_string(),
            "--serial",
            SERIAL,
            "--access-code",
            CODE,
            "--model",
            "a1mini",
            "gcode",
            "G28",
            "--confirm",
        ])
        .env_remove("BAMBU_IP")
        .env_remove("BAMBU_MQTT_PORT")
        .current_dir(std::env::temp_dir())
        .output()
        .expect("running bambu gcode");
    assert!(
        out.status.success(),
        "the relay should acknowledge a command it took for the game:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Long enough for the release to be due — the press goes out at once, the
    // release when the hold runs out — and only then hang up. Killing serve
    // first would take the keyboard thread with it and leave the key down.
    std::thread::sleep(Duration::from_millis(500));
    drop(serve);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        // `cat` writes what it has, so a 2-byte read is a press whose release
        // is still on its way rather than a release that never came.
        if let Ok(seen) = std::fs::read(&keys)
            && seen.len() >= 4
        {
            // KEY_FIRE down, KEY_FIRE up.
            assert_eq!(
                seen,
                vec![1, 0xa3, 0, 0xa3],
                "the home button should have reached the engine as fire"
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "nothing ever reached the engine's keyboard"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
