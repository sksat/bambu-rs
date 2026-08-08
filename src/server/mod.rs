//! The embedded HTTP server — a monitoring + control API (axum) and, when the
//! `dashboard` feature is on, the React SPA (embedded via `rust-embed`). Behind
//! the `server` feature; another consumer of the library, like the CLI.
//!
//! Auth: reads are always open; writes (control) are gated by an optional
//! password (`None` = open). The LAN access code stays server-side and never
//! reaches a client.

pub mod api;
#[cfg(feature = "dashboard")]
pub mod assets;
pub mod camera;
pub mod control;
#[cfg(feature = "relay")]
pub mod emulate;
pub mod files;
#[cfg(feature = "relay")]
pub mod ftpd;
pub mod live;
#[cfg(feature = "relay")]
pub mod relay;
pub mod start;
pub mod stream_record;
pub mod timelapse;

use std::sync::Arc;
use std::time::Duration;

use std::collections::BTreeMap;

use crate::config::ResolvedTarget;
pub use api::{
    FakeSource, PrinterSource, PrinterState, ServerState, is_safe_printer_id, printer_id,
    printer_id_key,
};
pub use camera::{CameraSource, ExternalCamera, LiveCamera, NoCamera};
pub use control::{Controller, FakeController, LiveController};
#[cfg(feature = "relay")]
use emulate::Upstream as _;
pub use files::{FakeFiles, FileStore, LiveFiles};
pub use live::LiveSource;
pub use start::{FakeStarter, LiveStarter, Starter};

/// Options for [`serve`].
pub struct ServeOpts {
    /// Bind host (default `127.0.0.1`). A non-loopback host serves over the
    /// network; without a password, control is open — a warning is printed.
    pub host: String,
    pub port: u16,
    /// Optional password gating **write** (control) requests. `None` = control is
    /// open. Reads are always unauthenticated.
    pub password: Option<String>,
    /// Serve deterministic fake data instead of talking to a printer.
    pub fake: bool,
    pub interval: Option<Duration>,
    /// External IP cameras to seed at launch (each a single-JPEG-per-GET URL with
    /// a label). The server proxies them via `/api/camera/{id}/snapshot` so a
    /// browser that can't reach the LAN cam (e.g. over Tailscale) still gets a live
    /// view; the dashboard can add/remove more at runtime.
    pub external_cameras: Vec<ExternalCamera>,
    /// Emulate a Local-Mode printer so Bambu Studio (and any other LAN client)
    /// can connect *through* this server instead of fighting it for the
    /// printer's attention. `None` = off.
    #[cfg(feature = "relay")]
    pub emulate: Option<EmulateOpts>,
}

#[cfg(feature = "relay")]
/// How often the relay refreshes its cache from the printer when the user did
/// not ask for a poll interval. Gentle — the printer caps `pushall` at about
/// 1/s, and this is one client's worth of polling no matter how many are
/// connected through the relay.
const EMULATE_REFRESH: Duration = Duration::from_secs(20);

/// Where the emulated printer listens, and what it will pass through.
#[cfg(feature = "relay")]
pub struct EmulateOpts {
    /// Bind host for the emulated MQTT listener. Defaults to loopback like the
    /// rest of `serve`; a client on another machine needs `0.0.0.0`.
    pub host: String,
    /// Bind port. `8883` is where a client looks unless told otherwise.
    pub port: u16,
    /// Where the emulated printer's FTP server listens (`990` by convention),
    /// or `None` to serve MQTT only.
    ///
    /// Without it a client can watch and control a print but not *start* one:
    /// Bambu Studio uploads the sliced file over FTP first, and that upload goes
    /// to whichever host it was pointed at.
    pub ftp_port: Option<u16>,
    /// Serve reads but refuse anything that would move or heat the machine —
    /// and, on the FTP side, anything that writes.
    pub read_only: bool,
}

/// A printer to serve, under the profile name it is configured as.
///
/// `Debug` redacts through `ResolvedTarget`'s own impl, which never prints the
/// access code.
#[derive(Debug)]
pub struct ServeTarget {
    pub name: String,
    pub target: ResolvedTarget,
}

/// Run the server (blocking; owns its own multi-thread runtime).
///
/// `targets` is every printer to serve; the first is the default — the one that
/// also answers on the unprefixed `/api/...` paths. An empty list (or `--fake`)
/// serves a single fake printer, which is what the dashboard's E2E suite runs
/// against.
pub fn serve(targets: Vec<ServeTarget>, opts: ServeOpts) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    #[cfg(feature = "relay")]
    let emulate = opts.emulate;
    let ServeOpts {
        host,
        port,
        password,
        fake,
        interval,
        external_cameras,
        ..
    } = opts;
    // There is nothing to relay to. Better to say so than to stand up a listener
    // that answers every read with an empty snapshot.
    #[cfg(feature = "relay")]
    if emulate.is_some() && (fake || targets.is_empty()) {
        anyhow::bail!(
            "--emulate relays a real printer; it has nothing to serve with --fake or without a \
             configured printer"
        );
    }
    // One emulated printer per host, not per port: a client looks for a printer
    // at `IP:8883` and cannot generally be told otherwise, and the emulator
    // ships no SSDP responder, so two of them need two addresses on this
    // machine — not two ports.
    #[cfg(feature = "relay")]
    if emulate.is_some() && targets.len() > 1 {
        anyhow::bail!(
            "--emulate serves one printer; run a second `bambu serve --printer <name> --emulate` \
             bound to another address on this host (--emulate-host)"
        );
    }
    // Two profiles for the same machine would open two MQTT clients to it, and
    // on an A1 those mutually disconnect — leaving BOTH feeds unreliable, which
    // looks like a flaky printer rather than a configuration mistake.
    //
    // Checked on the address as well as the serial: the broker is per-address,
    // so two profiles that agree on the IP are the same machine even when one
    // of their serials is a typo — which is exactly the case a serial-only
    // check would wave through.
    /// What makes two profiles the same machine, and what to call it.
    type SameMachine = (&'static str, fn(&ServeTarget) -> &str);
    let keys: [SameMachine; 2] = [
        ("serial", |t| t.target.serial.as_str()),
        ("address", |t| t.target.ip.as_str()),
    ];
    for (what, key) in keys {
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for t in &targets {
            if let Some(first) = seen.insert(key(t), &t.name) {
                anyhow::bail!(
                    "profiles {first:?} and {:?} are the same printer (same {what} {}); serving \
                     both would open two connections to it, which on an A1 disconnects them both",
                    t.name,
                    key(t),
                );
            }
        }
    }
    // Two names that sanitise to the same identifier would share a route and a
    // capture directory. Refuse rather than pick a winner — and compare them
    // the way a case-insensitive filesystem would, since this crate ships macOS
    // and Windows builds where `captures/A1` and `captures/a1` are one
    // directory.
    let mut by_id: BTreeMap<String, (String, String)> = BTreeMap::new();
    for t in &targets {
        let id = printer_id(&t.name);
        if let Some((first, first_id)) =
            by_id.insert(printer_id_key(&id), (t.name.clone(), id.clone()))
        {
            anyhow::bail!(
                "profiles {first:?} and {:?} are addressed as {first_id:?} and {id:?}, which name \
                 the same capture directory on a case-insensitive filesystem; rename one",
                t.name
            );
        }
    }
    rt.block_on(async move {
        // Seeded from flags that name no printer, so they belong to the default
        // one; the rest start empty and gain cameras at runtime through
        // `/api/printers/<name>/camera/config`.
        let mut seed_cameras = Some(external_cameras);
        let mut printers = BTreeMap::new();
        let mut default = String::new();
        if targets.is_empty() || fake {
            if !fake {
                eprintln!(
                    "note: no printer configured; serving fake data (pass --fake to silence)"
                );
            }
            let tick = interval.unwrap_or(Duration::from_secs(1));
            let name = targets
                .first()
                .map_or_else(|| "fake".to_string(), |t| t.name.clone());
            let id = printer_id(&name);
            default = id.clone();
            printers.insert(
                id.clone(),
                PrinterState {
                    name,
                    id,
                    model: None,
                    legacy_captures: true,
                    source: Arc::new(FakeSource::ramping(tick)),
                    controller: Arc::new(FakeController::verified()),
                    files: Arc::new(FakeFiles),
                    starter: Arc::new(FakeStarter),
                    password: password.clone(),
                    start_lock: Arc::new(tokio::sync::Mutex::new(())),
                    external_cameras: Arc::new(std::sync::RwLock::new(
                        seed_cameras.take().unwrap_or_default(),
                    )),
                    internal_camera: Arc::new(NoCamera),
                    timelapse: Default::default(),
                },
            );
        } else {
            eprintln!("connecting to the printer over LAN…");
            for (i, ServeTarget { name, target: t }) in targets.into_iter().enumerate() {
                let id = printer_id(&name);
                if i == 0 {
                    default = id.clone();
                }
                // The seeded cameras come from flags that name no printer, so
                // they go to the default one; the rest start empty and are
                // added at runtime through `/api/printers/<name>/camera/config`.
                let cams = seed_cameras.take().unwrap_or_default();
                let source = connect_source(
                    &t,
                    interval,
                    #[cfg(feature = "relay")]
                    &emulate,
                )
                .await?;
                printers.insert(
                    id.clone(),
                    PrinterState {
                        model: Some(t.model.to_string()),
                        // The default printer inherits the runs recorded before
                        // captures were namespaced; they were all its own.
                        legacy_captures: i == 0,
                        name,
                        id,
                        source,
                        controller: Arc::new(LiveController::new(t.clone())),
                        files: Arc::new(LiveFiles::new(t.clone())),
                        starter: Arc::new(LiveStarter::new(t.clone())),
                        password: password.clone(),
                        start_lock: Arc::new(tokio::sync::Mutex::new(())),
                        external_cameras: Arc::new(std::sync::RwLock::new(cams)),
                        internal_camera: Arc::new(LiveCamera::new(t)),
                        timelapse: Default::default(),
                    },
                );
            }
        }
        let state = ServerState {
            printers: Arc::new(printers),
            default,
        };
        let addr = format!("{host}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;
        let loopback = host.starts_with("127.") || host == "localhost" || host == "::1";
        if !loopback {
            match &password {
                Some(_) => eprintln!(
                    "warning: serving on non-loopback {addr}; control requires the password, \
                     reads are open."
                ),
                None => eprintln!(
                    "warning: serving on non-loopback {addr} with no --password — control \
                     (pause/stop/light/speed) is OPEN to anyone who can reach this address."
                ),
            }
        }
        eprintln!("bambu serve: http://{addr}/");
        axum::serve(listener, api::router(state))
            .await
            .map_err(|e| anyhow::anyhow!("serving: {e}"))
    })
}

/// Open this printer's status feed.
///
/// With emulation on, the dashboard reads off the same link the relay uses:
/// standing up a second connection here is exactly the contention `--emulate`
/// exists to remove.
async fn connect_source(
    t: &ResolvedTarget,
    interval: Option<Duration>,
    #[cfg(feature = "relay")] emulate: &Option<EmulateOpts>,
) -> anyhow::Result<Arc<dyn PrinterSource>> {
    #[cfg(not(feature = "relay"))]
    {
        Ok(Arc::new(LiveSource::connect(t.clone(), interval)))
    }
    #[cfg(feature = "relay")]
    match emulate {
        None => Ok(Arc::new(LiveSource::connect(t.clone(), interval))),
        Some(em) => {
            // Both subscribers first, then connect: the connection's opening
            // `pushall` is the seed for every cached view, and a broadcast with
            // no receivers throws it away.
            let link = relay::LivePrinterLink::new();
            let source = Arc::new(LiveSource::from_reports(link.subscribe()));
            start_emulator(t, em, Arc::clone(&link)).await?;
            // The relay answers clients' `pushall`s from its cache rather than
            // forwarding them, so nothing else would ever refresh it: seeded
            // once at connect and then fed only deltas, which are QoS 0 and
            // therefore lossy. One lost delta would leave the cache subtly
            // wrong for the rest of the print. A poll bounds that, and the
            // printer sees one client's worth of it however many are watching.
            let interval = interval.or(Some(EMULATE_REFRESH));
            Arc::clone(&link).connect(t.clone(), interval);
            Ok(source)
        }
    }
}

/// Render `host:port` for a human, bracketing an IPv6 literal.
///
/// Only for messages — the listeners bind with the `(host, port)` tuple, because
/// formatting an IPv6 host into a string produces `::1:8883`, which parses as
/// nothing.
#[cfg(feature = "relay")]
fn show_addr(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Bind the emulated printer's MQTT listener and start serving it.
#[cfg(feature = "relay")]
///
/// Bound before the HTTP server so a port clash (something else on 8883, or a
/// second `bambu serve`) fails at startup rather than after the dashboard has
/// come up and looked healthy.
async fn start_emulator(
    target: &ResolvedTarget,
    opts: &EmulateOpts,
    link: Arc<relay::LivePrinterLink>,
) -> anyhow::Result<()> {
    use crate::core::emulate::EmulatedPrinter;

    let printer = EmulatedPrinter::new(&target.serial, &target.access_code);
    let printer = if opts.read_only {
        printer.read_only()
    } else {
        printer
    };
    let tls = crate::tls::emulated_printer_server_config(&target.serial)?;
    // The tuple form, not "{host}:{port}": formatting an IPv6 host makes `::1`
    // into the unparseable `::1:8883`, and the loopback notice below explicitly
    // recognises `::1` as a host someone may pass.
    let addr = show_addr(&opts.host, opts.port);
    let listener = tokio::net::TcpListener::bind((opts.host.as_str(), opts.port))
        .await
        .map_err(|e| anyhow::anyhow!("binding the emulated printer on {addr}: {e}"))?;

    // Both listeners bound before either is served, so a port clash on FTP
    // fails at startup rather than after MQTT has come up looking healthy.
    let ftp = match opts.ftp_port {
        Some(port) => Some(bind_ftp_relay(target, opts, port).await?),
        None => None,
    };

    let emulator = emulate::Emulator::new(printer, link);
    tokio::spawn(Arc::clone(&emulator).pump());
    tokio::spawn({
        let tls = Arc::clone(&tls);
        async move {
            if let Err(e) = emulator.serve(listener, tls).await {
                eprintln!("emulate: listener stopped: {e}");
            }
        }
    });
    if let Some((relay, ftp_listener, ftp_addr)) = ftp {
        tokio::spawn({
            let tls = Arc::clone(&tls);
            async move {
                if let Err(e) = relay.serve(ftp_listener, tls).await {
                    eprintln!("emulate-ftp: listener stopped: {e}");
                }
            }
        });
        eprintln!("emulate: FTP relay on ftps://{ftp_addr} (implicit TLS)");
    }

    eprintln!(
        "emulating printer {} on mqtts://{addr} — point a client at this host \
         with the printer's own serial and access code",
        target.serial
    );
    if opts.ftp_port.is_none() {
        eprintln!(
            "emulate: no FTP relay, so a client can watch and control this printer but \
             not send it a print"
        );
    }
    if opts.read_only {
        eprintln!("emulate: read-only — control commands are refused, not forwarded");
    }
    if opts.host.starts_with("127.") || opts.host == "localhost" || opts.host == "::1" {
        eprintln!(
            "emulate: bound to loopback, so only clients on this machine can reach it; \
             pass --emulate-host 0.0.0.0 for the LAN"
        );
    } else {
        eprintln!(
            "emulate: reachable from the LAN. Anyone with the printer's access code can \
             drive it through this relay — the same people who could drive the printer \
             directly, and no others."
        );
    }
    Ok(())
}

/// Bind the FTP relay's listener, explaining the one failure people actually hit.
#[cfg(feature = "relay")]
async fn bind_ftp_relay(
    target: &ResolvedTarget,
    opts: &EmulateOpts,
    port: u16,
) -> anyhow::Result<(Arc<ftpd::FtpRelay>, tokio::net::TcpListener, String)> {
    let files: Arc<dyn ftpd::PrinterFiles> = Arc::new(ftpd::LivePrinterFiles::new(target.clone()));
    let relay = if opts.read_only {
        ftpd::FtpRelay::read_only(&target.access_code, files)
    } else {
        ftpd::FtpRelay::new(&target.access_code, files)
    };
    let addr = show_addr(&opts.host, port);
    let listener = tokio::net::TcpListener::bind((opts.host.as_str(), port))
        .await
        .map_err(|e| {
            // 990 is where an FTPS client looks, and it is below 1024, so this is
            // the error nearly everyone meets first. Naming the fixes beats making
            // them work out that the port is the problem.
            if e.kind() == std::io::ErrorKind::PermissionDenied && port < 1024 {
                anyhow::anyhow!(
                    "binding the FTP relay on {addr}: permission denied. Port {port} is \
                 privileged — grant the binary the capability once with \
                 `sudo setcap cap_net_bind_service=+ep $(which bambu)`, run as root, or \
                 pick an unprivileged port with --emulate-ftp-port (the client must be \
                 told the same one)."
                )
            } else {
                anyhow::anyhow!("binding the FTP relay on {addr}: {e}")
            }
        })?;
    Ok((relay, listener, addr))
}
