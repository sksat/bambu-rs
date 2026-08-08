//! Local-Mode printer emulation — the I/O half.
//!
//! Listens on MQTT-over-TLS the way a printer in LAN Mode does, so Bambu Studio
//! (or OrcaSlicer, or Home Assistant) can be pointed at this host instead of at
//! the machine. Everything those clients say is relayed over the **one**
//! connection `bambu serve` already holds, however many of them are watching.
//!
//! "One connection" is true of the *streaming* path, which is the one that used
//! to be held open permanently. It is not yet true of everything: the dashboard's
//! own control, file and job-start calls still connect per operation
//! ([`LiveController`](super::control::LiveController) and friends), so pressing
//! a button in the dashboard while emulating still opens a short-lived second
//! connection. That is the same wart as running the CLI while `serve` is up, and
//! it goes away when those move onto the link too.
//!
//! The protocol decisions all live in [`crate::core::emulate`] and
//! [`crate::core::mqtt`], where they are driven by unit tests without a socket.
//! What is here is the part that genuinely needs I/O: accepting TLS, framing
//! bytes into packets, and the fan-out.
//!
//! This is the MQTT half. Sending a print also needs the FTP half
//! ([`super::ftpd`]), because Bambu Studio uploads the sliced file before it
//! says `project_file`.
//!
//! **One known divergence.** A real printer puts everything on one ordered
//! stream, but here a command's ACK travels on its client's own channel while
//! reports travel on the fan-out, so a client can see its ACK slightly out of
//! order against the surrounding reports. That is the price of the ACK not being
//! droppable when a client lags, and it is self-correcting: state converges on
//! the next report either way.
//!
//! **No SSDP responder**, deliberately. It would be easy, and it would be wrong:
//! the real printer is announcing the same serial on the same LAN, so a client
//! would see two of it and pick whichever answered first. Add the relay by IP.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

use crate::core::emulate::{
    ClientId, ClientSession, EmulatedPrinter, Route, SequenceRewriter, UpstreamCache,
    report_publish,
};
use crate::core::mqtt::{self, Packet};

/// How many report messages a slow client may fall behind before it is resynced
/// from the cache instead of fed the backlog.
const FANOUT_DEPTH: usize = 256;

/// Refuse a packet larger than this rather than buffer it. A `pushall` snapshot
/// is a few kilobytes; anything approaching this is a client trying to exhaust
/// our memory, not a printer conversation.
const MAX_PACKET: usize = 4 * 1024 * 1024;

/// Read granularity off the socket.
const READ_CHUNK: usize = 8 * 1024;

/// How many ACKs may be queued for one client before we give up on it. ACKs are
/// rare and small; this deep a queue means the client has stopped reading.
const DIRECT_DEPTH: usize = 64;

/// How long the printer may go silent before the relay treats it as gone.
///
/// An A1 pushes a delta every couple of seconds unprompted, so silence this long
/// means the link is down — and the relay reconnects forever without telling
/// anyone. Left alone, a client would keep being served a cached picture of a
/// machine that has been off for an hour: Studio showing "RUNNING 41%", a Home
/// Assistant automation firing on an hour-old temperature. Dropping the
/// connection instead is the one signal every client already understands.
const STALE_AFTER: Duration = Duration::from_secs(30);

/// How often the watchdog checks. Coarse on purpose: this decides when to call a
/// printer dead, and being a second late costs nothing.
const WATCHDOG_TICK: Duration = Duration::from_secs(1);

/// How long a connection may take to get through TLS and say CONNECT.
///
/// Both happen before we know who is on the other end, so both are reachable by
/// anyone who can open a socket. Without a bound, a peer that connects and then
/// says nothing holds a task until the OS gives up on the TCP connection, which
/// can be hours — and it costs the peer nothing to do it again.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// Most connections either listener will serve at once.
///
/// TLS costs nothing to start and needs no access code, so without a bound a
/// peer that can merely reach the port can hold tasks, file descriptors and up
/// to `MAX_PACKET` of buffer each, simply by connecting and going quiet. The
/// printer this fronts has its own firmware limits; the relay needs its own.
/// Far above any real setup — a slicer, a dashboard and a home-automation
/// integration is three.
const MAX_CONNECTIONS: usize = 64;

/// The payload that asks the printer for a full snapshot.
fn pushall_request() -> Value {
    serde_json::json!({"pushing": {"sequence_id": "0", "command": "pushall"}})
}

/// Where the relay's reports come from and its requests go. Implemented by the
/// live printer link, and by a test double so the whole emulator can be driven
/// without a machine.
pub trait Upstream: Send + Sync + 'static {
    /// Every report message the printer sends, raw and unmerged.
    fn subscribe(&self) -> broadcast::Receiver<Value>;
    /// Publish a request payload to the printer. Best-effort: a request handed
    /// to a broken connection is lost, exactly as it would be if the client had
    /// been talking to the printer directly.
    fn send(&self, payload: Value);
}

/// The emulated printer: a TLS MQTT listener in front of one upstream link.
pub struct Emulator {
    printer: EmulatedPrinter,
    upstream: Arc<dyn Upstream>,
    cache: Arc<RwLock<UpstreamCache>>,
    rewriter: Arc<Mutex<SequenceRewriter>>,
    /// The printer's own pushes, pre-encoded once and shared. Lossy by design:
    /// a client too far behind is resynced from the cache rather than fed a
    /// backlog of stale deltas.
    fanout: broadcast::Sender<Arc<Vec<u8>>>,
    /// Per-client channels for messages addressed to exactly one client — ACKs.
    /// These do *not* go through the fan-out: an ACK dropped for lagging is
    /// unrecoverable, because routing it already consumed its mapping.
    clients: Mutex<HashMap<ClientId, mpsc::Sender<Arc<Vec<u8>>>>>,
    /// Fields, not constants, so a test can drive the timeouts without taking
    /// half a minute over it.
    handshake_timeout: Duration,
    stale_after: Duration,
    /// When the printer last said anything. The watchdog reads it; the pump
    /// writes it.
    last_report: Mutex<tokio::time::Instant>,
    /// `false` once the printer is presumed gone. Client tasks watch this and
    /// hang up, which is how the silence reaches them.
    healthy: tokio::sync::watch::Sender<bool>,
    next_client_id: AtomicU64,
    /// Subscribed here rather than in [`pump`](Self::pump), so reports arriving
    /// between construction and the pump task actually being scheduled are not
    /// dropped on the floor — a broadcast channel discards anything sent while
    /// it has no receivers.
    reports: Mutex<Option<broadcast::Receiver<Value>>>,
}

/// The two timeouts worth varying, so a test needn't wait out the real ones.
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// How long an unauthenticated peer gets to finish TLS and CONNECT.
    pub handshake_timeout: Duration,
    /// How long the printer may go silent before it is presumed gone.
    pub stale_after: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            handshake_timeout: HANDSHAKE_TIMEOUT,
            stale_after: STALE_AFTER,
        }
    }
}

impl Emulator {
    pub fn new(printer: EmulatedPrinter, upstream: Arc<dyn Upstream>) -> Arc<Self> {
        Self::with_tuning(printer, upstream, Tuning::default())
    }

    /// As [`new`](Self::new), with the timeouts set explicitly.
    pub fn with_tuning(
        printer: EmulatedPrinter,
        upstream: Arc<dyn Upstream>,
        tuning: Tuning,
    ) -> Arc<Self> {
        let (fanout, _) = broadcast::channel(FANOUT_DEPTH);
        let reports = Mutex::new(Some(upstream.subscribe()));
        let (healthy, _) = tokio::sync::watch::channel(true);
        Arc::new(Self {
            printer,
            upstream,
            cache: Arc::new(RwLock::new(UpstreamCache::new())),
            rewriter: Arc::new(Mutex::new(SequenceRewriter::new())),
            fanout,
            clients: Mutex::new(HashMap::new()),
            handshake_timeout: tuning.handshake_timeout,
            stale_after: tuning.stale_after,
            // Started now, not at epoch: the link needs a moment to connect, and
            // a relay that declared the printer dead before its first report
            // would hang up on whoever was waiting for it.
            last_report: Mutex::new(tokio::time::Instant::now()),
            healthy,
            next_client_id: AtomicU64::new(1),
            reports,
        })
    }

    /// Watch for the printer going quiet, and tell everyone when it has.
    ///
    /// Spawned by [`pump`](Self::pump). Runs forever; recovers by itself when
    /// the printer comes back, because the pump clears the state it sets.
    async fn watchdog(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(WATCHDOG_TICK);
        loop {
            ticker.tick().await;
            let silent_for = self
                .last_report
                .lock()
                .expect("last_report lock poisoned")
                .elapsed();
            if silent_for >= self.stale_after && *self.healthy.borrow() {
                eprintln!(
                    "emulate: nothing from the printer for {}s — treating it as offline and \
                     disconnecting clients rather than serving them a stale picture",
                    silent_for.as_secs()
                );
                self.cache
                    .write()
                    .expect("cache lock poisoned")
                    .mark_stale();
                // `send` would refuse when no client is connected, leaving the
                // flag saying "healthy" — and the next client to arrive would be
                // treated as online against a printer that is not.
                self.healthy.send_replace(false);
            }
        }
    }

    /// Consume the upstream report stream forever: merge it into the cache, then
    /// route it to the client(s) it belongs to. Spawn this once.
    pub async fn pump(self: Arc<Self>) {
        let mut reports = self
            .reports
            .lock()
            .expect("reports lock poisoned")
            .take()
            .unwrap_or_else(|| self.upstream.subscribe());
        tokio::spawn(Arc::clone(&self).watchdog());
        loop {
            let message = match reports.recv().await {
                Ok(m) => m,
                // We fell behind the printer and those deltas are gone. Merging
                // the rest onto a cache with a hole in it would serve a subtly
                // wrong picture indefinitely — nothing else refreshes it, since
                // a client's pushall is answered from the cache rather than
                // forwarded. Ask for a full snapshot instead.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!(
                        "emulate: fell {n} reports behind the printer; asking for a snapshot"
                    );
                    self.upstream.send(pushall_request());
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            };
            // The printer spoke, so it is alive: reset the watchdog and, if we
            // had given up on it, tell the clients it is back.
            *self.last_report.lock().expect("last_report lock poisoned") =
                tokio::time::Instant::now();
            if !*self.healthy.borrow() {
                eprintln!("emulate: the printer is answering again");
                self.healthy.send_replace(true);
            }
            // `apply` clears the stale flag, so reads start being served again.
            self.cache
                .write()
                .expect("cache lock poisoned")
                .apply(&message);
            // Cache first, then route: a client's pushall must never be answered
            // from a picture older than the report that just arrived.
            let route = self
                .rewriter
                .lock()
                .expect("rewriter lock poisoned")
                .route(&message);
            match route {
                Route::Broadcast(m) => {
                    // Err just means nobody is connected right now.
                    let _ = self.fanout.send(Arc::new(self.encode_report(&m)));
                }
                Route::ToClient { client, message } => {
                    let bytes = Arc::new(self.encode_report(&message));
                    let sender = self
                        .clients
                        .lock()
                        .expect("clients lock poisoned")
                        .get(&client)
                        .cloned();
                    // `try_send`, not `send`: the pump is the single reader of
                    // the printer's stream, and blocking it on one wedged socket
                    // would stall every other client. A missing sender just
                    // means the client asked and then left before the printer
                    // answered.
                    if let Some(tx) = sender
                        && let Err(e) = tx.try_send(bytes)
                    {
                        eprintln!("emulate: client {client} cannot take its ACK: {e}");
                    }
                }
            }
        }
    }

    fn encode_report(&self, message: &Value) -> Vec<u8> {
        mqtt::encode(&Packet::Publish(report_publish(
            self.printer.serial(),
            message,
        )))
    }

    /// Accept connections forever, each in its own task.
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        tls: Arc<rustls::ServerConfig>,
    ) -> anyhow::Result<()> {
        let acceptor = tokio_rustls::TlsAcceptor::from(tls);
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(x) => x,
                // One failed accept (fd exhaustion, a peer that vanished) must
                // not take the listener down with it.
                Err(e) => {
                    eprintln!("emulate: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            // Taken before the task is spawned, and held for its whole life, so
            // the cap bounds connections rather than merely slowing them down.
            let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
                eprintln!("emulate: refusing {peer}: already serving {MAX_CONNECTIONS} clients");
                drop(socket);
                continue;
            };
            let acceptor = acceptor.clone();
            let this = Arc::clone(&self);
            tokio::spawn(async move {
                let _slot = slot;
                let stream =
                    match tokio::time::timeout(this.handshake_timeout, acceptor.accept(socket))
                        .await
                    {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            eprintln!("emulate: TLS handshake with {peer} failed: {e}");
                            return;
                        }
                        Err(_) => {
                            eprintln!("emulate: {peer} opened a connection and never started TLS");
                            return;
                        }
                    };
                let id = this.next_client_id.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = Arc::clone(&this).serve_client(stream, id).await {
                    eprintln!("emulate: client {peer} dropped: {e}");
                }
                // Whatever it was still waiting on, nobody is there to receive it.
                this.rewriter
                    .lock()
                    .expect("rewriter lock poisoned")
                    .forget_client(id);
                this.clients
                    .lock()
                    .expect("clients lock poisoned")
                    .remove(&id);
            });
        }
    }

    /// Serve one client until it disconnects or misbehaves.
    ///
    /// Generic over the stream so this can be driven over anything duplex; in
    /// production it is always a TLS stream.
    async fn serve_client<S>(self: Arc<Self>, stream: S, id: ClientId) -> anyhow::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let mut fanout = self.fanout.subscribe();
        let (direct_tx, mut direct) = mpsc::channel(DIRECT_DEPTH);
        self.clients
            .lock()
            .expect("clients lock poisoned")
            .insert(id, direct_tx);
        let mut session = ClientSession::new();
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; READ_CHUNK];
        let mut healthy = self.healthy.subscribe();
        // Check the value NOW as well as marking it seen: `changed()` only
        // reports transitions, so a client arriving while the printer is
        // already known to be offline would otherwise sit connected forever,
        // never having missed an edge.
        if !*healthy.borrow_and_update() {
            anyhow::bail!("the printer is offline; refusing the connection");
        }
        // Absolute, and held ACROSS iterations. Recreating the timeout inside
        // the select each time round would restart it every time any other arm
        // fired — and the fan-out arm fires for every client on every report,
        // about once a second, so nothing would ever time out.
        //
        // The handshake budget runs from the moment the connection opened and is
        // never extended: a peer that dribbles bytes without ever completing a
        // CONNECT would otherwise hold a slot forever by staying just inside the
        // window.
        let connect_by = tokio::time::Instant::now() + self.handshake_timeout;
        let mut idle_deadline = Some(connect_by);

        loop {
            tokio::select! {
                // Bytes from the client.
                read = reader.read(&mut chunk) => {
                    let n = read?;
                    if n == 0 {
                        return Ok(()); // clean EOF
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    // One read can carry several packets, or half of one.
                    let mut decoded_a_packet = false;
                    while let Some((packet, used)) = mqtt::decode(&buf)? {
                        decoded_a_packet = true;
                        buf.drain(..used);
                        let response = {
                            let cache = self.cache.read().expect("cache lock poisoned");
                            session.handle(packet, &self.printer, &cache)
                        };
                        for out in &response.send {
                            writer.write_all(&mqtt::encode(out)).await?;
                        }
                        for payload in response.upstream {
                            // Rewritten so this client's `sequence_id` can't be
                            // confused with another's — see SequenceRewriter.
                            let rewritten = self
                                .rewriter
                                .lock()
                                .expect("rewriter lock poisoned")
                                .rewrite_request(id, &payload);
                            self.upstream.send(rewritten);
                        }
                        if response.close {
                            writer.flush().await?;
                            return Ok(());
                        }
                    }
                    // Checked on what is LEFT after the complete packets have
                    // been taken out: a client legitimately pipelining more than
                    // the cap in one read is fine, a client dribbling bytes that
                    // never form a packet is not.
                    if buf.len() > MAX_PACKET {
                        anyhow::bail!(
                            "client is holding {} bytes that do not form a packet",
                            buf.len()
                        );
                    }
                    writer.flush().await?;
                    // Only a COMPLETE packet counts as the client speaking —
                    // MQTT's keep-alive is about control packets, and bytes that
                    // never form one are exactly what the deadline is for. Until
                    // CONNECT lands, the original absolute budget stands.
                    if session.is_connected() {
                        if decoded_a_packet {
                            idle_deadline = keep_alive_deadline(&session);
                        }
                    } else {
                        idle_deadline = Some(connect_by);
                    }
                }

                // An ACK for something this client asked for. Directed rather
                // than broadcast: a client that lagged past the fan-out's depth
                // would lose it, and the rewriter has already given up the
                // mapping by then, so nothing could ever re-send it — leaving
                // Bambu Studio waiting forever on a `pause` it really did issue.
                Some(bytes) = direct.recv() => {
                    writer.write_all(&bytes).await?;
                    writer.flush().await?;
                }

                // The printer went away. Hang up rather than keep answering out
                // of a cache that no longer describes anything — a dropped
                // connection is what every client already reads as "offline".
                changed = healthy.changed() => {
                    if changed.is_err() || !*healthy.borrow_and_update() {
                        anyhow::bail!("the printer stopped answering; disconnecting");
                    }
                }

                // Nothing from this client for too long.
                () = idle_after(idle_deadline) => {
                    anyhow::bail!(
                        "silent past its deadline ({})",
                        if session.is_connected() { "keep-alive" } else { "handshake" }
                    );
                }

                // Reports on their way out.
                message = fanout.recv() => {
                    match message {
                        Ok(m) => {
                            if !session.wants_reports() {
                                continue; // hasn't subscribed; a real printer would send it nothing
                            }
                            writer.write_all(&m).await?;
                            writer.flush().await?;
                        }
                        // This client couldn't keep up. Rather than leave it
                        // holding a state built from deltas with a hole in it,
                        // hand it a fresh full snapshot and let it resync.
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!("emulate: client {id} fell {n} reports behind; resyncing");
                            // Take the snapshot and drop the guard before the
                            // write: a lock held across an await would make this
                            // task non-Send, and would block the pump on a slow
                            // socket.
                            let resync = session.wants_reports().then(|| {
                                self.cache.read().expect("cache lock poisoned").snapshot_reply()
                            }).flatten();
                            if let Some(snapshot) = resync {
                                writer.write_all(&self.encode_report(&snapshot)).await?;
                                writer.flush().await?;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
            }
        }
    }
}

/// How long a connected session may go without a control packet.
///
/// MQTT 3.1.1 §3.1.2.10 lets the server disconnect a client that sends nothing
/// for 1.5× its own keep-alive, which is what catches a client that vanished
/// without a FIN — an unplugged laptop — long before the OS does. A client that
/// asked for keep-alive `0` asked for no timeout, and gets none.
fn keep_alive_limit(session: &ClientSession) -> Option<Duration> {
    match session.keep_alive() {
        0 => None,
        secs => Some(Duration::from_millis(u64::from(secs) * 1500)),
    }
}

/// The same limit as an absolute instant, so it survives being carried across
/// loop iterations instead of restarting whenever another branch fires.
fn keep_alive_deadline(session: &ClientSession) -> Option<tokio::time::Instant> {
    keep_alive_limit(session).map(|d| tokio::time::Instant::now() + d)
}

/// Complete at `deadline`, or never if there isn't one.
async fn idle_after(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SERIAL: &str = "0309FATESTSERIAL";
    const CODE: &str = "12345678";

    /// An upstream nobody is really behind: the test pushes reports in and reads
    /// back whatever the emulator forwarded.
    struct FakeUpstream {
        reports: broadcast::Sender<Value>,
        sent: Mutex<Vec<Value>>,
    }

    impl FakeUpstream {
        fn new() -> Arc<Self> {
            let (reports, _) = broadcast::channel(64);
            Arc::new(Self {
                reports,
                sent: Mutex::new(Vec::new()),
            })
        }
        fn push(&self, message: Value) {
            let _ = self.reports.send(message);
        }
        fn forwarded(&self) -> Vec<Value> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl Upstream for FakeUpstream {
        fn subscribe(&self) -> broadcast::Receiver<Value> {
            self.reports.subscribe()
        }
        fn send(&self, payload: Value) {
            self.sent.lock().unwrap().push(payload);
        }
    }

    /// A snapshot the emulator can answer a pushall from.
    fn snapshot() -> Value {
        json!({"print": {
            "command": "push_status", "msg": 0, "sequence_id": "812",
            "gcode_state": "RUNNING", "mc_percent": 40, "nozzle_temper": 220.0,
        }})
    }

    /// Start an emulator on an ephemeral port; returns its address.
    async fn start(upstream: Arc<FakeUpstream>) -> (Arc<Emulator>, std::net::SocketAddr) {
        start_as(EmulatedPrinter::new(SERIAL, CODE), upstream).await
    }

    async fn start_as(
        printer: EmulatedPrinter,
        upstream: Arc<FakeUpstream>,
    ) -> (Arc<Emulator>, std::net::SocketAddr) {
        start_tuned(printer, upstream, Tuning::default()).await
    }

    async fn start_tuned(
        printer: EmulatedPrinter,
        upstream: Arc<FakeUpstream>,
        tuning: Tuning,
    ) -> (Arc<Emulator>, std::net::SocketAddr) {
        let emulator = Emulator::with_tuning(printer, upstream as Arc<dyn Upstream>, tuning);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = crate::tls::emulated_printer_server_config(SERIAL).unwrap();
        tokio::spawn(Arc::clone(&emulator).pump());
        tokio::spawn(Arc::clone(&emulator).serve(listener, tls));
        (emulator, addr)
    }

    /// Poll `check` until it holds, failing the test rather than hanging if it
    /// never does. Everything here crosses tasks, so a bare loop turns a bug
    /// into a hung suite.
    async fn until(what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// A minimal MQTT client over TLS, so the test drives real bytes through the
    /// real handshake rather than calling the state machine directly.
    struct TestClient {
        stream: tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
        buf: Vec<u8>,
    }

    impl TestClient {
        async fn connect(addr: std::net::SocketAddr) -> Self {
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let connector =
                tokio_rustls::TlsConnector::from(crate::tls::lan_client_config().unwrap());
            let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
            let stream = connector.connect(name, tcp).await.unwrap();
            Self {
                stream,
                buf: Vec::new(),
            }
        }

        async fn send(&mut self, packet: Packet) {
            self.stream.write_all(&mqtt::encode(&packet)).await.unwrap();
            self.stream.flush().await.unwrap();
        }

        /// Read the next packet, failing the test rather than hanging forever.
        async fn recv(&mut self) -> Packet {
            let mut chunk = [0u8; 4096];
            loop {
                if let Some((packet, used)) = mqtt::decode(&self.buf).unwrap() {
                    self.buf.drain(..used);
                    return packet;
                }
                let n = tokio::time::timeout(Duration::from_secs(5), self.stream.read(&mut chunk))
                    .await
                    .expect("timed out waiting for a packet")
                    .unwrap();
                assert_ne!(n, 0, "the emulator closed the connection");
                self.buf.extend_from_slice(&chunk[..n]);
            }
        }

        async fn handshake(&mut self, password: &str) -> Packet {
            self.send(Packet::Connect(crate::core::mqtt::Connect {
                client_id: "test-client".into(),
                username: Some("bblp".into()),
                password: Some(password.into()),
                keep_alive: 0,
                clean_session: true,
            }))
            .await;
            self.recv().await
        }

        async fn subscribe_reports(&mut self) {
            self.send(Packet::Subscribe {
                packet_id: 1,
                filters: vec![(format!("device/{SERIAL}/report"), 0)],
            })
            .await;
            assert!(matches!(self.recv().await, Packet::SubAck { .. }));
        }

        async fn publish_request(&mut self, payload: Value) {
            self.send(Packet::Publish(crate::core::mqtt::Publish::at_most_once(
                format!("device/{SERIAL}/request"),
                payload.to_string().into_bytes(),
            )))
            .await;
        }

        /// The JSON of the next report publish.
        async fn recv_report(&mut self) -> Value {
            match self.recv().await {
                Packet::Publish(p) => serde_json::from_slice(&p.payload).unwrap(),
                other => panic!("expected a report publish, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_client_completes_the_tls_handshake_and_authenticates() {
        // The end-to-end proof that the generated certificate, the codec and the
        // session agree: real TLS, real bytes, no printer.
        let (_e, addr) = start(FakeUpstream::new()).await;
        let mut client = TestClient::connect(addr).await;
        assert_eq!(
            client.handshake(CODE).await,
            Packet::ConnAck {
                session_present: false,
                code: crate::core::mqtt::ConnectCode::Accepted
            }
        );
    }

    #[tokio::test]
    async fn a_wrong_access_code_is_refused_over_the_wire() {
        let (_e, addr) = start(FakeUpstream::new()).await;
        let mut client = TestClient::connect(addr).await;
        assert_eq!(
            client.handshake("00000000").await,
            Packet::ConnAck {
                session_present: false,
                code: crate::core::mqtt::ConnectCode::BadCredentials
            }
        );
    }

    #[tokio::test]
    async fn a_report_from_the_printer_reaches_a_subscribed_client() {
        let upstream = FakeUpstream::new();
        let (_e, addr) = start(Arc::clone(&upstream)).await;
        let mut client = TestClient::connect(addr).await;
        client.handshake(CODE).await;
        // Once the SUBACK is in hand the subscription is live, so a single push
        // is enough — no polling, and a lost report fails instead of retrying.
        client.subscribe_reports().await;
        upstream.push(snapshot());
        assert_eq!(
            client.recv_report().await["print"]["gcode_state"],
            "RUNNING"
        );
    }

    #[tokio::test]
    async fn two_clients_both_see_the_same_report() {
        // The whole point: the printer has one connection, and both of these
        // are watching it.
        let upstream = FakeUpstream::new();
        let (_e, addr) = start(Arc::clone(&upstream)).await;
        let mut a = TestClient::connect(addr).await;
        let mut b = TestClient::connect(addr).await;
        a.handshake(CODE).await;
        b.handshake(CODE).await;
        a.subscribe_reports().await;
        b.subscribe_reports().await;

        upstream.push(json!({"print": {"command": "push_status", "msg": 1, "mc_percent": 42}}));
        assert_eq!(a.recv_report().await["print"]["mc_percent"], 42);
        assert_eq!(b.recv_report().await["print"]["mc_percent"], 42);
    }

    #[tokio::test]
    async fn a_pushall_is_answered_from_the_cache_without_touching_the_printer() {
        let upstream = FakeUpstream::new();
        let (emulator, addr) = start(Arc::clone(&upstream)).await;
        upstream.push(snapshot());
        until("the cache to warm up", || {
            emulator.cache.read().unwrap().is_warm()
        })
        .await;

        let mut client = TestClient::connect(addr).await;
        client.handshake(CODE).await;
        client.subscribe_reports().await;
        client
            .publish_request(json!({"pushing": {"sequence_id": "1", "command": "pushall"}}))
            .await;

        let v = client.recv_report().await;
        assert_eq!(v["print"]["msg"], 0, "a full snapshot, not a delta");
        assert_eq!(v["print"]["gcode_state"], "RUNNING");
        assert!(
            upstream.forwarded().is_empty(),
            "the printer should never have seen it: {:?}",
            upstream.forwarded()
        );
    }

    #[tokio::test]
    async fn a_control_command_reaches_the_printer_with_a_rewritten_sequence_id() {
        let upstream = FakeUpstream::new();
        let (_e, addr) = start(Arc::clone(&upstream)).await;
        let mut client = TestClient::connect(addr).await;
        client.handshake(CODE).await;
        client.subscribe_reports().await;
        client
            .publish_request(json!({"print": {"sequence_id": "1", "command": "pause"}}))
            .await;

        until("the command to reach the printer", || {
            !upstream.forwarded().is_empty()
        })
        .await;
        let forwarded = upstream.forwarded();
        assert_eq!(forwarded[0]["print"]["command"], "pause");
        assert_ne!(
            forwarded[0]["print"]["sequence_id"], "1",
            "the id must be made unique before it reaches the printer"
        );

        // …and the printer's ACK comes back wearing the client's own id.
        let upstream_id = forwarded[0]["print"]["sequence_id"].as_str().unwrap();
        upstream.push(json!({"print": {
            "sequence_id": upstream_id, "command": "pause", "result": "success",
        }}));
        let ack = client.recv_report().await;
        assert_eq!(ack["print"]["sequence_id"], "1");
        assert_eq!(ack["print"]["result"], "success");
    }

    #[tokio::test]
    async fn a_read_only_relay_refuses_control_but_still_serves_reads() {
        let upstream = FakeUpstream::new();
        let (_e, addr) = start_as(
            EmulatedPrinter::new(SERIAL, CODE).read_only(),
            Arc::clone(&upstream),
        )
        .await;

        let mut client = TestClient::connect(addr).await;
        client.handshake(CODE).await;
        client.subscribe_reports().await;
        client
            .publish_request(json!({"print": {"sequence_id": "1", "command": "stop"}}))
            .await;

        let ack = client.recv_report().await;
        assert_eq!(ack["print"]["result"], "fail");
        assert!(
            ack["print"]["reason"]
                .as_str()
                .unwrap()
                .contains("read-only")
        );
        assert!(
            upstream.forwarded().is_empty(),
            "nothing reached the machine"
        );
    }

    #[tokio::test]
    async fn a_third_party_client_implementation_can_talk_to_it() {
        // Every other test here drives the emulator with the emulator's own
        // codec, which would happily agree with itself about a wrong byte.
        // rumqttc is an independent implementation — the same one this crate
        // uses against real printers — so if the wire format is off, this is
        // what notices.
        let upstream = FakeUpstream::new();
        let (emulator, addr) = start(Arc::clone(&upstream)).await;
        upstream.push(snapshot());
        until("the cache to warm up", || {
            emulator.cache.read().unwrap().is_warm()
        })
        .await;

        let mut opts =
            rumqttc::MqttOptions::new("rumqttc-probe", addr.ip().to_string(), addr.port());
        opts.set_credentials("bblp", CODE);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_transport(rumqttc::Transport::Tls(rumqttc::TlsConfiguration::Rustls(
            crate::tls::lan_client_config().unwrap(),
        )));
        let (client, mut eventloop) = rumqttc::AsyncClient::new(opts, 16);
        client
            .subscribe(format!("device/{SERIAL}/report"), rumqttc::QoS::AtMostOnce)
            .await
            .unwrap();
        // Ordered behind the SUBSCRIBE on the same connection, so by the time
        // this is handled the subscription is live and the cache answers it.
        client
            .publish(
                format!("device/{SERIAL}/request"),
                rumqttc::QoS::AtLeastOnce,
                false,
                json!({"pushing": {"sequence_id": "0", "command": "pushall"}}).to_string(),
            )
            .await
            .unwrap();

        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), eventloop.poll())
                .await
                .expect("rumqttc should reach a report before this times out")
                .expect("the connection should stay up");
            if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(p)) = event {
                let v: Value = serde_json::from_slice(&p.payload).unwrap();
                assert_eq!(p.topic, format!("device/{SERIAL}/report"));
                assert_eq!(v["print"]["msg"], 0);
                assert_eq!(v["print"]["gcode_state"], "RUNNING");
                return;
            }
        }
    }

    #[test]
    fn a_connected_client_gets_the_keep_alive_it_asked_for() {
        // The anonymous side is an absolute budget driven end-to-end by
        // `dribbling_bytes_does_not_extend_the_handshake_window`; this pins the
        // arithmetic for a connected one, which a 90-second test could not.
        let printer = EmulatedPrinter::new(SERIAL, CODE);
        let cache = UpstreamCache::new();

        // Before CONNECT there is no keep-alive to honour; the handshake budget
        // is what governs, and it is not derived from the session.
        assert_eq!(keep_alive_limit(&ClientSession::new()), None);

        let mut connected = ClientSession::new();
        connected.handle(
            Packet::Connect(crate::core::mqtt::Connect {
                client_id: "c".into(),
                username: Some("bblp".into()),
                password: Some(CODE.into()),
                keep_alive: 60,
                clean_session: true,
            }),
            &printer,
            &cache,
        );
        // 1.5x the client's own keep-alive, per §3.1.2.10.
        assert_eq!(keep_alive_limit(&connected), Some(Duration::from_secs(90)));

        let mut no_keepalive = ClientSession::new();
        no_keepalive.handle(
            Packet::Connect(crate::core::mqtt::Connect {
                client_id: "c".into(),
                username: Some("bblp".into()),
                password: Some(CODE.into()),
                keep_alive: 0,
                clean_session: true,
            }),
            &printer,
            &cache,
        );
        // 0 means the client asked for no timeout, and gets none.
        assert_eq!(keep_alive_limit(&no_keepalive), None);
    }

    #[tokio::test]
    async fn a_silent_anonymous_peer_is_dropped_even_while_reports_are_flowing() {
        // The bug this pins: the idle timeout used to be rebuilt inside the
        // select on every iteration, so any other arm firing restarted it. The
        // fan-out arm fires for every client on every report — about once a
        // second from a real printer — so the deadline never arrived and anyone
        // who could open a socket could hold a task and an fd indefinitely.
        let upstream = FakeUpstream::new();
        let emulator = Emulator::with_tuning(
            EmulatedPrinter::new(SERIAL, CODE),
            Arc::clone(&upstream) as Arc<dyn Upstream>,
            Tuning {
                handshake_timeout: Duration::from_millis(300),
                ..Tuning::default()
            },
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = crate::tls::emulated_printer_server_config(SERIAL).unwrap();
        tokio::spawn(Arc::clone(&emulator).pump());
        tokio::spawn(Arc::clone(&emulator).serve(listener, tls));

        // Reports the whole time, so the fan-out arm keeps waking the task.
        let chatter = tokio::spawn(async move {
            loop {
                upstream.push(snapshot());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });

        let mut client = TestClient::connect(addr).await;
        // TLS is up, and we say nothing at all — never even CONNECT.
        let mut chunk = [0u8; 64];
        let closed = tokio::time::timeout(Duration::from_secs(5), client.stream.read(&mut chunk))
            .await
            .expect("the emulator should have hung up long before this");
        // Dropped mid-session, so the peer sees either a clean EOF or a torn
        // TLS stream depending on timing — both mean "disconnected". What must
        // NOT happen is bytes: that would be reports going to an unauthenticated
        // peer, on a connection that was supposed to be gone.
        assert!(
            matches!(closed, Ok(0) | Err(_)),
            "a peer that never authenticates must be disconnected, got {closed:?}"
        );
        chatter.abort();
    }

    #[tokio::test]
    async fn a_printer_that_goes_quiet_disconnects_its_clients_instead_of_faking_it() {
        // The relay reconnects to a dropped printer forever and says nothing.
        // Without this, a client keeps its connection and keeps being answered
        // from cache: Studio showing "RUNNING 41%" for a machine that has been
        // off for an hour, an automation firing on an hour-old temperature.
        let upstream = FakeUpstream::new();
        let (emulator, addr) = start_tuned(
            EmulatedPrinter::new(SERIAL, CODE),
            Arc::clone(&upstream),
            Tuning {
                stale_after: Duration::from_millis(300),
                ..Tuning::default()
            },
        )
        .await;
        upstream.push(snapshot());
        until("the cache to warm up", || {
            emulator.cache.read().unwrap().is_warm()
        })
        .await;

        let mut client = TestClient::connect(addr).await;
        client.handshake(CODE).await;
        client.subscribe_reports().await;

        // …and now the printer says nothing at all, ever again.
        let mut chunk = [0u8; 64];
        let closed = tokio::time::timeout(Duration::from_secs(5), client.stream.read(&mut chunk))
            .await
            .expect("the client should have been disconnected");
        assert!(
            matches!(closed, Ok(0) | Err(_)),
            "a dropped connection is how a client learns the printer is gone, got {closed:?}"
        );
        // The cache stops answering too, so a *new* client can't be told a
        // comfortable lie either.
        assert!(!emulator.cache.read().unwrap().is_warm());
    }

    #[tokio::test]
    async fn the_printer_coming_back_makes_the_relay_serve_again() {
        // Recovery has to be automatic: nobody is going to restart serve.
        let upstream = FakeUpstream::new();
        let (emulator, addr) = start_tuned(
            EmulatedPrinter::new(SERIAL, CODE),
            Arc::clone(&upstream),
            Tuning {
                stale_after: Duration::from_millis(300),
                ..Tuning::default()
            },
        )
        .await;
        upstream.push(snapshot());
        until("the cache to warm", || {
            emulator.cache.read().unwrap().is_warm()
        })
        .await;
        until("the printer to be given up on", || {
            !emulator.cache.read().unwrap().is_warm()
        })
        .await;

        // It comes back.
        upstream.push(snapshot());
        until("the relay to serve again", || {
            emulator.cache.read().unwrap().is_warm()
        })
        .await;

        // A client connecting now is served normally.
        let mut client = TestClient::connect(addr).await;
        client.handshake(CODE).await;
        client.subscribe_reports().await;
        client
            .publish_request(json!({"pushing": {"sequence_id": "1", "command": "pushall"}}))
            .await;
        assert_eq!(
            client.recv_report().await["print"]["gcode_state"],
            "RUNNING"
        );
    }

    #[tokio::test]
    async fn dribbling_bytes_does_not_extend_the_handshake_window() {
        // The deadline used to reset on any read at all, so a peer could send
        // one byte before each expiry and hold a connection slot — and, after
        // CONNECT, sidestep the keep-alive it negotiated — without ever
        // completing a packet. The handshake budget is absolute now.
        let upstream = FakeUpstream::new();
        let (_e, addr) = start_tuned(
            EmulatedPrinter::new(SERIAL, CODE),
            Arc::clone(&upstream),
            Tuning {
                handshake_timeout: Duration::from_millis(400),
                ..Tuning::default()
            },
        )
        .await;

        let mut client = TestClient::connect(addr).await;
        // A PUBLISH header declaring the largest legal remaining length, then a
        // byte at a time forever. This is the shape that matters: never a
        // complete packet, but never a malformed one either, so nothing except
        // the deadline can end it.
        let dribble = tokio::spawn(async move {
            if client
                .stream
                .write_all(&[0x30, 0xFF, 0xFF, 0xFF, 0x7F])
                .await
                .is_err()
            {
                return client;
            }
            loop {
                if client.stream.write_all(b"x").await.is_err() {
                    return client;
                }
                let _ = client.stream.flush().await;
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        });

        // Well past several deadlines' worth of dribbling.
        let mut client = tokio::time::timeout(Duration::from_secs(5), dribble)
            .await
            .expect("the peer should have been disconnected, not fed more time")
            .unwrap();
        let mut chunk = [0u8; 16];
        let closed = client.stream.read(&mut chunk).await;
        assert!(
            matches!(closed, Ok(0) | Err(_)),
            "the connection should be gone, got {closed:?}"
        );
    }

    #[tokio::test]
    async fn a_ping_keeps_the_connection_alive() {
        let (_e, addr) = start(FakeUpstream::new()).await;
        let mut client = TestClient::connect(addr).await;
        client.handshake(CODE).await;
        client.send(Packet::PingReq).await;
        assert_eq!(client.recv().await, Packet::PingResp);
    }
}
