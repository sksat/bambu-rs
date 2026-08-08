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

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SERIAL: &str = "E2ETESTSERIAL01";
const CODE: &str = "13572468";

/// A child process killed when the test ends, however it ends.
struct Proc {
    child: Child,
    name: &'static str,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Only interesting when something went wrong; a passing run says nothing.
        if std::thread::panicking()
            && let Some(mut err) = self.child.stderr.take()
        {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            eprintln!("--- {} stderr ---\n{s}", self.name);
        }
    }
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bambu"))
}

/// A port nothing is listening on. Bind, read the number, drop — a classic race
/// in principle, and in practice the only way to get an ephemeral port without
/// threading a listener through a child process.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn(name: &'static str, args: &[&str]) -> Proc {
    let child = bin()
        .args(args)
        // A developer's real printer must not leak in and turn this into a
        // test against hardware.
        .env_remove("BAMBU_IP")
        .env_remove("BAMBU_SERIAL")
        .env_remove("BAMBU_ACCESS_CODE")
        .env_remove("BAMBU_MODEL")
        .env_remove("BAMBU_MQTT_PORT")
        .env_remove("BAMBU_FTPS_PORT")
        .env(
            "XDG_CONFIG_HOME",
            std::env::temp_dir().join("bambu-e2e-none"),
        )
        .current_dir(std::env::temp_dir()) // away from the repo's own .env
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawning {name}: {e}"));
    Proc { child, name }
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

    // 1. The printer: synthetic, nothing connected to it.
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
            &printer_http.to_string(),
            "--emulate",
            "--emulate-host",
            "127.0.0.1",
            "--emulate-port",
            &printer_mqtt.to_string(),
            "--emulate-ftp-port",
            &printer_ftp.to_string(),
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
