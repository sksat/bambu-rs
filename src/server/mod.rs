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
#[cfg(feature = "relay")]
pub mod camerad;
pub mod control;
#[cfg(feature = "relay")]
pub mod detect;
#[cfg(feature = "relay")]
pub mod emulate;
pub mod files;
#[cfg(feature = "relay")]
pub mod ftpd;
pub mod hook;
pub mod live;
#[cfg(feature = "relay")]
pub mod relay;
pub mod slice;
pub mod start;
pub mod stream_record;
#[cfg(feature = "relay")]
pub mod synthetic;
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
pub use hook::{LiveHook, NoHook, PrePrint, PrePrintHook};
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
    /// Explicit slicer binary, instead of auto-detecting one. Given, it must
    /// exist — an explicit choice never silently falls back to another install.
    pub slicer_bin: Option<std::path::PathBuf>,
    /// Explicit `.../profiles/BBL` bundle to slice from.
    pub slicer_profiles: Option<std::path::PathBuf>,
    /// Wall-clock cap on one slice. Unlike a physical tuning constant this HAS a
    /// default, because a wrong value cannot cause an unsafe action: too small
    /// only fails the slice.
    pub slice_timeout: Duration,
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
    /// Passive data ports for the FTP relay, as `"first-last"`. `None` = any
    /// ephemeral port, which a deny-by-default firewall will block.
    pub pasv_ports: Option<String>,
    /// The plain device-detect port (`3000` on a real printer), or `None` to
    /// leave it unserved.
    ///
    /// Bambu Studio's "add printer by IP" probes this *before* MQTT and gives up
    /// silently if nothing answers, so without it the relay cannot be added by
    /// IP at all — however well it serves 8883.
    pub detect_port: Option<u16>,
    /// The TLS device-detect port (`3002`), or `None`. Same protocol inside.
    pub detect_tls_port: Option<u16>,
    /// Present a **substituted** chamber camera on the printer's camera port.
    ///
    /// `None` leaves 6000 unserved, and a client then sees no camera — which is
    /// the truth. Showing one machine's video as another's is deliberate enough
    /// that it has to be asked for by name; it never follows from an external
    /// camera merely being configured.
    pub camera: Option<EmulateCamera>,
    /// The address to tell clients the printer is at.
    ///
    /// The printer's report carries its own LAN address, and a client believes
    /// it: Bambu Studio takes the camera and the file upload there, whatever
    /// address it was given for MQTT. Relaying that unchanged makes the relay
    /// carry the session and nothing else.
    ///
    /// `None` when the bind host already names a reachable address — it is then
    /// derived from it. Binding to `0.0.0.0` is a binding instruction rather
    /// than a place, so that case has to say which address clients should use.
    pub advertise: Option<std::net::Ipv4Addr>,
    /// Serve reads but refuse anything that would move or heat the machine —
    /// and, on the FTP side, anything that writes.
    pub read_only: bool,
}

/// Which camera stands in for the printer's own, and where it is served.
#[cfg(feature = "relay")]
pub struct EmulateCamera {
    /// The label of a `--camera-url` / `--cameras-config` entry.
    pub label: String,
    /// Bind port. `6000` is where a client looks unless told otherwise.
    pub port: u16,
    /// How often to fetch, for a camera that only offers single snapshots.
    ///
    /// Required in that case and with no default: the right rate belongs to the
    /// camera and the network between here and it. A camera with a `stream_url`
    /// ignores this — its frames arrive when they arrive.
    pub poll: Option<Duration>,
}

/// A printer to serve, under the profile name it is configured as.
///
/// `Debug` redacts through `ResolvedTarget`'s own impl, which never prints the
/// access code.
#[derive(Debug)]
pub struct ServeTarget {
    pub name: String,
    pub target: ResolvedTarget,
    /// This printer's `pre_print` sequence, already resolved from its profile.
    /// Resolved by the caller because `hooks`/`sequences` live on `Profile`,
    /// which nothing under `src/server/` sees.
    pub hook: Option<PrePrint>,
}

/// Parse a `"first-last"` passive port range.
///
/// A hard error rather than a shrug: someone passing this is working around a
/// firewall, and silently ignoring a typo would leave them debugging transfers
/// that hang for reasons the relay could have named at startup.
#[cfg(feature = "relay")]
pub fn parse_pasv_ports(spec: &str) -> anyhow::Result<std::ops::RangeInclusive<u16>> {
    let (first, last) = spec.split_once('-').ok_or_else(|| {
        anyhow::anyhow!("passive port range must look like 50000-50100, got {spec:?}")
    })?;
    let first: u16 = first
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{first:?} is not a port number"))?;
    let last: u16 = last
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{last:?} is not a port number"))?;
    if first == 0 || last < first {
        anyhow::bail!("passive port range {first}-{last} is empty or starts at zero");
    }
    Ok(first..=last)
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
        slicer_bin,
        slicer_profiles,
        slice_timeout,
        ..
    } = opts;
    // There has to be *a* printer, real or synthetic, and an identity to present
    // it under. Without either, the relay would answer every read with an empty
    // snapshot, which looks to a client like a printer gone strange rather than
    // one that was never there.
    #[cfg(feature = "relay")]
    if emulate.is_some() && targets.is_empty() {
        anyhow::bail!(
            "--emulate needs a printer to present: configure one, or pass --fake with \
             --serial/--access-code to relay a synthetic one"
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
        // One slicer for the whole server: it is a single heavyweight binary on
        // this host, not a per-printer resource. A host without one is NOT a
        // reason to refuse to serve — the API says so and degrades, like the
        // missing-ffmpeg path.
        let slicer: Arc<dyn slice::Slicer> = if fake {
            Arc::new(slice::FakeSlicer)
        } else {
            match slice::LiveSlicer::discover(slicer_bin, slicer_profiles, slice_timeout) {
                Ok(s) => Arc::new(s),
                Err(reason) => {
                    eprintln!("note: slicing unavailable — {reason}");
                    Arc::new(slice::NoSlicer(reason))
                }
            }
        };
        let registry = crate::core::capability::default_registry();
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
            // `--fake --emulate`: the same relay, in front of a printer that
            // isn't there. It is what lets the whole stack be exercised across
            // processes with no hardware — so the dashboard reads the synthetic
            // printer's own reports rather than a second, unrelated fake.
            #[cfg(feature = "relay")]
            let cams = seed_cameras.take().unwrap_or_default();
            #[cfg(feature = "relay")]
            let fake_cams = cams.clone();
            #[cfg(not(feature = "relay"))]
            let fake_cams = seed_cameras.take().unwrap_or_default();
            #[cfg(feature = "relay")]
            let source: Arc<dyn PrinterSource> = match (&emulate, targets.first()) {
                (Some(em), Some(ServeTarget { target: t, .. })) => {
                    let printer = synthetic::SyntheticPrinter::start(tick);
                    let source = Arc::new(LiveSource::from_reports(printer.subscribe()));
                    start_emulator(
                        t,
                        em,
                        printer,
                        synthetic::SyntheticFiles::new(),
                        // Nothing to ask, so the identity is composed from the
                        // one we were told to present under.
                        Arc::new(detect::SyntheticDetect {
                            serial: t.serial.clone(),
                            // An unrecognised model reports whatever it was
                            // configured as, rather than being rounded to some
                            // real machine it is not.
                            model: t
                                .model
                                .device_code()
                                .unwrap_or_else(|| t.model.as_str())
                                .to_string(),
                        }),
                        &cams,
                    )
                    .await?;
                    source
                }
                _ => Arc::new(FakeSource::ramping(tick)),
            };
            #[cfg(not(feature = "relay"))]
            let source: Arc<dyn PrinterSource> = Arc::new(FakeSource::ramping(tick));
            printers.insert(
                id.clone(),
                PrinterState {
                    name,
                    id,
                    model: None,
                    legacy_captures: true,
                    source,
                    controller: Arc::new(FakeController::verified()),
                    files: Arc::new(FakeFiles),
                    starter: Arc::new(FakeStarter),
                    password: password.clone(),
                    start_lock: Arc::new(tokio::sync::Mutex::new(())),
                    external_cameras: Arc::new(std::sync::RwLock::new(fake_cams)),
                    internal_camera: Arc::new(NoCamera),
                    timelapse: Default::default(),
                    hook_running: Default::default(),
                    hook: Arc::new(NoHook),
                    slicer: Arc::clone(&slicer),
                    slice_jobs: Default::default(),
                    // The fake printer impersonates an A1 mini everywhere else
                    // (FakeSource, fake AMS), so it slices as one too — which is
                    // what `--fake` serves.
                    slicer_names: registry
                        .slicer_names(&crate::core::model::Model::A1Mini)
                        .copied(),
                },
            );
        } else {
            eprintln!("connecting to the printer over LAN…");
            for (
                i,
                ServeTarget {
                    name,
                    target: t,
                    hook,
                },
            ) in targets.into_iter().enumerate()
            {
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
                    #[cfg(feature = "relay")]
                    &cams,
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
                        internal_camera: Arc::new(LiveCamera::new(t.clone())),
                        timelapse: Default::default(),
                        hook_running: Default::default(),
                        hook: match hook {
                            Some(spec) => Arc::new(LiveHook::new(t.clone(), spec)),
                            None => Arc::new(NoHook),
                        },
                        // The machine profile follows THIS printer's model, so
                        // the unprefixed and per-printer routes cannot slice
                        // geometry for the wrong machine's bed.
                        slicer_names: registry.slicer_names(&t.model).copied(),
                        slicer: Arc::clone(&slicer),
                        slice_jobs: Default::default(),
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
    #[cfg(feature = "relay")] cameras: &[ExternalCamera],
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
            start_emulator(
                t,
                em,
                Arc::clone(&link) as Arc<dyn emulate::Upstream>,
                Arc::new(ftpd::LivePrinterFiles::new(t.clone())),
                // Ask the machine itself: model, name and firmware are facts
                // about it, not ours to compose.
                detect::ProxyDetect::new(&t.ip, t.detect_port),
                cameras,
            )
            .await?;
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
    upstream: Arc<dyn emulate::Upstream>,
    files: Arc<dyn ftpd::PrinterFiles>,
    detect_source: Arc<dyn detect::DetectSource>,
    cameras: &[ExternalCamera],
) -> anyhow::Result<()> {
    use crate::core::emulate::EmulatedPrinter;

    let printer = EmulatedPrinter::new(&target.serial, &target.access_code);
    let printer = if opts.read_only {
        printer.read_only()
    } else {
        printer
    };
    // Kept on disk, not made afresh: Bambu Studio verifies a printer against the
    // CAs it bundles, so a relay can only ever be trusted by being pinned — and a
    // pin is worthless against an identity that changes every restart.
    let cert_dir = crate::config::default_emulate_cert_dir();
    let tls = crate::tls::emulated_printer_server_config(&target.serial, cert_dir.as_deref())?;
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
        Some(port) => Some(bind_ftp_relay(target, opts, port, files).await?),
        None => None,
    };

    // Likewise the detect ports — and these especially, because a client that
    // cannot probe us does not report an error, it simply never arrives.
    let mut detect_listeners = Vec::new();
    for (port, tls) in [
        (opts.detect_port, None),
        (opts.detect_tls_port, Some(Arc::clone(&tls))),
    ] {
        let Some(port) = port else { continue };
        let addr = show_addr(&opts.host, port);
        let listener = tokio::net::TcpListener::bind((opts.host.as_str(), port))
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::PermissionDenied && port < 1024 {
                    anyhow::anyhow!(
                        "binding the detect listener on {addr}: permission denied — \
                         port {port} is privileged. Grant the capability once with \
                         `sudo setcap cap_net_bind_service=+ep $(which bambu)`, or move it \
                         with --emulate-detect-port (a client looking for a printer by IP \
                         only ever probes 3000/3002, so moving it means Studio won't find \
                         this relay)."
                    )
                } else {
                    anyhow::anyhow!("binding the detect listener on {addr}: {e}")
                }
            })?;
        detect_listeners.push((listener, tls, addr));
    }

    // Bound with the others, before anything is served: a camera port already
    // taken should stop startup, not surface later as a client that finds a
    // printer with no picture.
    let camera = match &opts.camera {
        Some(want) => Some(bind_camera_relay(target, opts, want, cameras).await?),
        None => None,
    };

    let emulator = emulate::Emulator::new(printer, upstream);
    if opts.camera.is_some() {
        emulator.claim_camera();
    }
    emulator.advertise_at(advertised_address(opts)?);
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
    if let Some((relay, listener, addr, from)) = camera {
        tokio::spawn({
            let tls = Arc::clone(&tls);
            async move {
                if let Err(e) = relay.serve(listener, tls).await {
                    eprintln!("emulate-camera: listener stopped: {e}");
                }
            }
        });
        eprintln!("emulate: chamber camera on {addr}, showing {from}");
    }
    for (listener, tls, addr) in detect_listeners {
        let source = Arc::clone(&detect_source);
        let kind = if tls.is_some() { "TLS" } else { "plain" };
        tokio::spawn(async move {
            if let Err(e) = detect::serve(listener, source, tls).await {
                eprintln!("emulate-detect: listener stopped: {e}");
            }
        });
        eprintln!("emulate: device-detect ({kind}) on {addr}");
    }
    if opts.camera.is_none() {
        eprintln!(
            "emulate: no camera relay, so a client's liveview will stay empty (pass \
             --emulate-camera <label> to show one of the configured cameras instead)"
        );
    }
    if opts.detect_port.is_none() && opts.detect_tls_port.is_none() {
        eprintln!(
            "emulate: no detect listener, so Bambu Studio's \"add printer by IP\" will not \
             find this relay — it probes 3000/3002 before MQTT and gives up quietly"
        );
    }

    eprintln!(
        "emulating printer {} on mqtts://{addr} — point a client at this host \
         with the printer's own serial and access code",
        target.serial
    );
    if let Some(dir) = &cert_dir {
        // Named because a client that checks certificates cannot be talked round
        // any other way: it has to be handed this file.
        eprintln!(
            "emulate: TLS identity {} (stable across restarts; a client that \
             verifies certificates must be pointed at it)",
            dir.join(format!("{}.cert.pem", target.serial)).display()
        );
    }
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

/// Which address to tell clients the printer is at.
///
/// Derived from the bind host when that names one, because a relay bound to a
/// specific address is reachable at it. `0.0.0.0` names no address at all, and
/// picking one of this machine's interfaces would be a guess that fails
/// silently — the client simply talks to the printer instead, and the relay
/// looks like it is working right up until the camera is empty.
#[cfg(feature = "relay")]
fn advertised_address(opts: &EmulateOpts) -> anyhow::Result<std::net::Ipv4Addr> {
    if let Some(ip) = opts.advertise {
        return Ok(ip);
    }
    if let Ok(ip) = opts.host.parse::<std::net::Ipv4Addr>()
        && !ip.is_unspecified()
    {
        return Ok(ip);
    }
    anyhow::bail!(
        "--emulate-host {} does not name an address clients can use, and the printer's \
         report tells them where to find it — without --emulate-advertise <IP> they would \
         take the camera and file transfers straight to the printer, past this relay",
        opts.host
    )
}

/// Bind the camera relay, resolving which camera is standing in.
///
/// Every way this can be wrong is a hard error rather than a fallback: a client
/// shown the wrong camera has no way to tell, and a silently-empty liveview is
/// indistinguishable from the printer's own camera being off.
#[cfg(feature = "relay")]
async fn bind_camera_relay(
    target: &ResolvedTarget,
    opts: &EmulateOpts,
    want: &EmulateCamera,
    cameras: &[ExternalCamera],
) -> anyhow::Result<(
    Arc<camerad::CameraRelay>,
    tokio::net::TcpListener,
    String,
    String,
)> {
    let camera = cameras
        .iter()
        .find(|c| c.label == want.label)
        .ok_or_else(|| {
            let known: Vec<&str> = cameras.iter().map(|c| c.label.as_str()).collect();
            if known.is_empty() {
                anyhow::anyhow!(
                    "--emulate-camera {:?} needs a camera to show; none are configured \
                     (add one with --camera-url or --cameras-config)",
                    want.label
                )
            } else {
                anyhow::anyhow!(
                    "--emulate-camera {:?} names no configured camera; there is {}",
                    want.label,
                    known.join(", ")
                )
            }
        })?;

    // A stream is a moving picture; polling a snapshot URL is a slideshow of
    // the same view. Prefer the stream, and refuse to invent a rate for the
    // camera that only has snapshots.
    let frames = match (&camera.stream_url, want.poll) {
        (Some(url), _) => camerad::FrameSource::from_mjpeg(camera::url_stream_opener(url.clone())),
        (None, Some(every)) => {
            let url = camera.url.clone();
            camerad::FrameSource::from_snapshots(
                Arc::new(move || camera::fetch_snapshot(&url)),
                every,
            )
        }
        (None, None) => anyhow::bail!(
            "camera {:?} offers single snapshots, not a stream, so --emulate-camera-interval \
             must say how often to take one",
            camera.label
        ),
    };

    let addr = show_addr(&opts.host, want.port);
    let listener = tokio::net::TcpListener::bind((opts.host.as_str(), want.port))
        .await
        .map_err(|e| anyhow::anyhow!("binding the camera relay on {addr}: {e}"))?;
    let from = match &camera.stream_url {
        Some(url) => format!("{} ({url})", camera.label),
        None => format!("{} ({}, polled)", camera.label, camera.url),
    };
    Ok((
        camerad::CameraRelay::new(&target.access_code, frames),
        listener,
        addr,
        from,
    ))
}

/// Bind the FTP relay's listener, explaining the one failure people actually hit.
#[cfg(feature = "relay")]
async fn bind_ftp_relay(
    target: &ResolvedTarget,
    opts: &EmulateOpts,
    port: u16,
    files: Arc<dyn ftpd::PrinterFiles>,
) -> anyhow::Result<(Arc<ftpd::FtpRelay>, tokio::net::TcpListener, String)> {
    let relay = if opts.read_only {
        ftpd::FtpRelay::read_only(&target.access_code, files)
    } else {
        ftpd::FtpRelay::new(&target.access_code, files)
    };
    let pasv = opts
        .pasv_ports
        .as_deref()
        .map(parse_pasv_ports)
        .transpose()?;
    if let Some(range) = &pasv {
        eprintln!(
            "emulate: FTP passive data ports confined to {}-{}",
            range.start(),
            range.end()
        );
    }
    let relay = relay.with_pasv_ports(pasv);
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
