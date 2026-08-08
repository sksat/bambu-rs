//! Local-Mode printer emulation — the I/O half.
//!
//! Listens on MQTT-over-TLS the way a printer in LAN Mode does, so Bambu Studio
//! (or OrcaSlicer, or Home Assistant) can be pointed at this host instead of at
//! the machine. Everything those clients say is relayed over the **one**
//! connection `bambu serve` already holds, so the printer sees a single client
//! no matter how many are watching.
//!
//! The protocol decisions all live in [`crate::core::emulate`] and
//! [`crate::core::mqtt`], where they are driven by unit tests without a socket.
//! What is here is the part that genuinely needs I/O: accepting TLS, framing
//! bytes into packets, and the fan-out.
//!
//! ## What this is not
//!
//! - **No FTPS server.** Bambu Studio uploads a sliced file to the printer's
//!   FTPS:990 before it sends `project_file`, so *sending a print* through the
//!   relay does not work yet — monitoring and control do. Point Studio's upload
//!   at the printer directly, or use `bambu job start`.
//! - **No SSDP responder.** It would be easy, and it would be wrong: the real
//!   printer is announcing the same serial on the same LAN, so a client would
//!   see two of it and pick whichever answered first. Add the relay by IP.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

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

/// One pre-encoded report, and who should get it.
enum Fanout {
    /// The printer's own push — everyone subscribed sees it.
    All(Arc<Vec<u8>>),
    /// An ACK, which belongs to the one client whose command it answers.
    One(ClientId, Arc<Vec<u8>>),
}

/// The emulated printer: a TLS MQTT listener in front of one upstream link.
pub struct Emulator {
    printer: EmulatedPrinter,
    upstream: Arc<dyn Upstream>,
    cache: Arc<RwLock<UpstreamCache>>,
    rewriter: Arc<Mutex<SequenceRewriter>>,
    fanout: broadcast::Sender<Arc<Fanout>>,
    next_client_id: AtomicU64,
    /// Subscribed here rather than in [`pump`](Self::pump), so reports arriving
    /// between construction and the pump task actually being scheduled are not
    /// dropped on the floor — a broadcast channel discards anything sent while
    /// it has no receivers.
    reports: Mutex<Option<broadcast::Receiver<Value>>>,
}

impl Emulator {
    pub fn new(printer: EmulatedPrinter, upstream: Arc<dyn Upstream>) -> Arc<Self> {
        let (fanout, _) = broadcast::channel(FANOUT_DEPTH);
        let reports = Mutex::new(Some(upstream.subscribe()));
        Arc::new(Self {
            printer,
            upstream,
            cache: Arc::new(RwLock::new(UpstreamCache::new())),
            rewriter: Arc::new(Mutex::new(SequenceRewriter::new())),
            fanout,
            next_client_id: AtomicU64::new(1),
            reports,
        })
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
        loop {
            let message = match reports.recv().await {
                Ok(m) => m,
                // We fell behind the printer. The cache still merges everything
                // that follows, so the gap closes itself on the next snapshot.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            };
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
            let fanout = match route {
                Route::Broadcast(m) => Fanout::All(Arc::new(self.encode_report(&m))),
                Route::ToClient { client, message } => {
                    Fanout::One(client, Arc::new(self.encode_report(&message)))
                }
            };
            // Err just means nobody is connected right now.
            let _ = self.fanout.send(Arc::new(fanout));
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
            let acceptor = acceptor.clone();
            let this = Arc::clone(&self);
            tokio::spawn(async move {
                let stream = match acceptor.accept(socket).await {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("emulate: TLS handshake with {peer} failed: {e}");
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
        let mut session = ClientSession::new();
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; READ_CHUNK];

        loop {
            tokio::select! {
                // Bytes from the client.
                read = read_with_keepalive(&mut reader, &mut chunk, session.keep_alive()) => {
                    let n = read?;
                    if n == 0 {
                        return Ok(()); // clean EOF
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > MAX_PACKET {
                        anyhow::bail!("client sent more than {MAX_PACKET} bytes without a complete packet");
                    }
                    // One read can carry several packets, or half of one.
                    while let Some((packet, used)) = mqtt::decode(&buf)? {
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
                    writer.flush().await?;
                }

                // Reports on their way out.
                message = fanout.recv() => {
                    match message {
                        Ok(m) => {
                            if !session.wants_reports() {
                                continue; // hasn't subscribed; a real printer would send it nothing
                            }
                            match &*m {
                                Fanout::All(bytes) => writer.write_all(bytes).await?,
                                Fanout::One(target, bytes) if *target == id => {
                                    writer.write_all(bytes).await?
                                }
                                Fanout::One(..) => continue,
                            }
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

/// Read, giving up if the client goes quiet for longer than MQTT allows.
///
/// §3.1.2.10: with a non-zero keep-alive the server may disconnect a client that
/// sends nothing for 1.5× that. Without it, a client that vanishes without a FIN
/// (an unplugged laptop) holds its slot until the OS notices, which can be
/// hours.
async fn read_with_keepalive<R>(
    reader: &mut R,
    chunk: &mut [u8],
    keep_alive: u16,
) -> anyhow::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    if keep_alive == 0 {
        return Ok(reader.read(chunk).await?);
    }
    let limit = Duration::from_millis(u64::from(keep_alive) * 1500);
    match tokio::time::timeout(limit, reader.read(chunk)).await {
        Ok(n) => Ok(n?),
        Err(_) => anyhow::bail!("silent for more than 1.5× the {keep_alive}s keep-alive"),
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
        let emulator = Emulator::new(printer, upstream as Arc<dyn Upstream>);
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

    #[tokio::test]
    async fn a_ping_keeps_the_connection_alive() {
        let (_e, addr) = start(FakeUpstream::new()).await;
        let mut client = TestClient::connect(addr).await;
        client.handshake(CODE).await;
        client.send(Packet::PingReq).await;
        assert_eq!(client.recv().await, Packet::PingResp);
    }
}
