//! Local-Mode printer emulation — the pure protocol half.
//!
//! `bambu serve` can pretend to *be* the printer, so Bambu Studio (or any other
//! LAN client) points at this host instead of at the machine. bambu-rs then holds
//! the one real LAN connection and fans it out: the contention that makes two
//! direct clients fight over the printer becomes N clients sharing one relay.
//!
//! Everything here is I/O-free. A [`ClientSession`] turns one downstream client's
//! [`Packet`]s into a [`Response`] — packets to write back, request payloads to
//! forward upstream, and whether to hang up. [`UpstreamCache`] holds the merged
//! view of the real printer so reads are answered locally, and
//! [`SequenceRewriter`] keeps two clients' identical `sequence_id`s from being
//! mistaken for each other.
//!
//! Faithfulness matters more than convenience: a client that talks to this must
//! not be able to tell (beyond the reads being *better* — a merged snapshot on
//! demand rather than deltas since whenever it connected).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::core::command::{report_topic, request_topic};
use crate::core::mqtt::{Connect, ConnectCode, Packet, Publish, topic_matches};
use crate::core::report::{ReportState, is_full_snapshot_message, is_status_report};

/// The username every Bambu LAN client connects with.
const LAN_USER: &str = "bblp";

/// How many recently acknowledged QoS-1 packet ids to remember per client, for
/// spotting a retransmission. Far more than any client keeps in flight, and far
/// fewer than the 65536 ids exist, so a legitimately recycled id is never
/// mistaken for a resend.
const RECENT_PACKET_IDS: usize = 256;

/// How many commands may be awaiting an ACK before the oldest are given up on.
/// Far above any real client's in-flight count — this is a leak backstop, not a
/// throttle.
pub const MAX_OUTSTANDING: usize = 1024;

/// Identifies one downstream client for the life of its connection. Assigned by
/// the I/O layer; only used to route an ACK back to whoever asked.
pub type ClientId = u64;

/// Whether the relay passes control commands through to the real machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPolicy {
    /// Forward everything — the point of the relay, and no weaker than the real
    /// printer, which accepts the same commands from anyone holding the code.
    Full,
    /// Serve reads, refuse anything that would move or heat the machine.
    ReadOnly,
    /// Serve reads; take every control command **here** and never forward it.
    ///
    /// Unlike [`ReadOnly`](Self::ReadOnly), which tells the client no, this
    /// accepts the command and hands it to whatever the I/O layer has put
    /// behind the relay — `serve --emulate-doom` puts a game there, and a jog
    /// becomes a step forward. The client is ACKed `success`, because from its
    /// side the command really was carried out; it just was not carried out by
    /// a printer.
    ///
    /// The forwarding is not merely skipped, it is unreachable: this arm writes
    /// to [`Response::intercepted`] and the upstream arm writes to
    /// [`Response::upstream`], and no path writes both. That is the whole
    /// safety argument for pointing a game at a relay — a command that plays
    /// DOOM cannot also move a machine.
    Intercept,
}

/// The identity and policy of the emulated printer.
///
/// The serial must be the **real** printer's: it is baked into the topic names a
/// client subscribes to, and a client configured for serial X will never listen
/// to serial Y's topic.
#[derive(Clone)]
pub struct EmulatedPrinter {
    serial: String,
    access_code: String,
    control: ControlPolicy,
}

impl std::fmt::Debug for EmulatedPrinter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmulatedPrinter")
            .field("serial", &self.serial)
            .field("access_code", &"<redacted>")
            .field("control", &self.control)
            .finish()
    }
}

impl EmulatedPrinter {
    /// A relay for `serial`, authenticating with `access_code`, control allowed.
    pub fn new(serial: impl Into<String>, access_code: impl Into<String>) -> Self {
        Self {
            serial: serial.into(),
            access_code: access_code.into(),
            control: ControlPolicy::Full,
        }
    }

    /// The same relay, but reads only.
    pub fn read_only(mut self) -> Self {
        self.control = ControlPolicy::ReadOnly;
        self
    }

    /// The same relay, with control taken here rather than forwarded — see
    /// [`ControlPolicy::Intercept`].
    ///
    /// Nothing in this module says what does the taking. The I/O layer pairs
    /// this with a sink, and the two are set together
    /// ([`crate::server::emulate::Emulator::intercepting`]) so a relay cannot
    /// end up quietly swallowing commands with nothing behind it.
    pub fn intercepted(mut self) -> Self {
        self.control = ControlPolicy::Intercept;
        self
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }

    pub fn control(&self) -> ControlPolicy {
        self.control
    }

    /// Check a client's credentials against the printer's.
    ///
    /// A plain comparison, not a constant-time one. The access code is 8 digits
    /// — 10^8 guesses, which a LAN attacker can simply make against the printer
    /// itself — so a timing oracle is not the cheap way in here, and pretending
    /// otherwise would be security theatre.
    fn authenticates(&self, c: &Connect) -> bool {
        c.username.as_deref() == Some(LAN_USER) && c.password.as_deref() == Some(&self.access_code)
    }

    fn report_topic(&self) -> String {
        report_topic(&self.serial)
    }

    fn request_topic(&self) -> String {
        request_topic(&self.serial)
    }
}

/// What the I/O layer must do after feeding one packet through a session.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Response {
    /// Packets to write back to **this** client, in order.
    pub send: Vec<Packet>,
    /// Request payloads to publish to the real printer.
    pub upstream: Vec<Value>,
    /// Control payloads taken by the relay itself under
    /// [`ControlPolicy::Intercept`] — consumed here, and *instead of*
    /// `upstream`, never as well as it.
    pub intercepted: Vec<Value>,
    /// Close the connection once `send` is flushed.
    pub close: bool,
}

impl Response {
    fn nothing() -> Self {
        Self::default()
    }

    fn send(packet: Packet) -> Self {
        Self {
            send: vec![packet],
            ..Self::default()
        }
    }

    /// Hang up. MQTT has no error packet, so for a protocol violation the close
    /// *is* the message.
    fn close() -> Self {
        Self {
            close: true,
            ..Self::default()
        }
    }
}

/// Where a session is in the MQTT handshake.
#[derive(Debug, PartialEq, Eq)]
enum Phase {
    AwaitingConnect,
    Connected,
}

/// One downstream client's connection state.
pub struct ClientSession {
    phase: Phase,
    /// Every accepted filter that covers this printer's report topic. A set
    /// rather than a flag because MQTT subscriptions are per-filter: a client
    /// holding both the exact topic and a wildcard over it, then dropping one,
    /// is still subscribed.
    report_filters: BTreeSet<String>,
    /// Set by `pushing.stop`, cleared by `pushing.start`. Separate from the
    /// subscription: the client is still subscribed, it has just asked not to
    /// be pushed to for now.
    push_paused: bool,
    /// QoS-1 packet ids acknowledged recently, oldest first. A client whose
    /// PUBACK went missing resends with DUP set; without this the command would
    /// be relayed a second time and the printer would run it twice.
    recent_packet_ids: std::collections::VecDeque<u16>,
    client_id: String,
    keep_alive: u16,
}

impl Default for ClientSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientSession {
    pub fn new() -> Self {
        Self {
            phase: Phase::AwaitingConnect,
            report_filters: BTreeSet::new(),
            push_paused: false,
            recent_packet_ids: std::collections::VecDeque::new(),
            client_id: String::new(),
            keep_alive: 0,
        }
    }

    /// Whether this client has subscribed to the report topic — i.e. whether the
    /// fan-out should reach it.
    pub fn wants_reports(&self) -> bool {
        !self.report_filters.is_empty() && !self.push_paused
    }

    /// Whether the handshake is done. Until it is, the peer is anonymous, which
    /// is what the I/O layer keys its stricter idle timeout on.
    pub fn is_connected(&self) -> bool {
        self.phase == Phase::Connected
    }

    /// The client id from CONNECT (empty before the handshake). For logging.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The client's requested keep-alive in seconds; `0` means "no timeout".
    /// MQTT 3.1.1 §3.1.2.10 says the server may disconnect after 1.5×.
    pub fn keep_alive(&self) -> u16 {
        self.keep_alive
    }

    /// Feed one decoded packet in and get the reaction out.
    pub fn handle(
        &mut self,
        packet: Packet,
        printer: &EmulatedPrinter,
        cache: &UpstreamCache,
    ) -> Response {
        // §3.1.0: the first packet MUST be CONNECT, and a second one is a
        // violation. Both are answered by closing, not by a reply.
        match (&self.phase, &packet) {
            (Phase::AwaitingConnect, Packet::Connect(_)) => {}
            (Phase::AwaitingConnect, _) | (Phase::Connected, Packet::Connect(_)) => {
                return Response::close();
            }
            _ => {}
        }

        match packet {
            Packet::Connect(c) => self.on_connect(c, printer),
            Packet::Subscribe { packet_id, filters } => {
                self.on_subscribe(packet_id, &filters, printer)
            }
            Packet::Unsubscribe { packet_id, filters } => {
                // Matched the same way SUBSCRIBE is, so a client that subscribed
                // with a wildcard can unsubscribe with the same one.
                let report = printer.report_topic();
                for f in &filters {
                    if topic_matches(f, &report) {
                        self.report_filters.remove(f);
                    }
                }
                Response::send(Packet::UnsubAck { packet_id })
            }
            Packet::Publish(p) => self.on_publish(p, printer, cache),
            Packet::PingReq => Response::send(Packet::PingResp),
            Packet::Disconnect => Response::close(),
            // A PUBACK for a report we pushed: nothing to do (we publish reports
            // at QoS 0, so this shouldn't arrive, but it isn't fatal).
            Packet::PubAck { .. } => Response::nothing(),
            // Server-to-client packets can't legitimately arrive here; the codec
            // already rejects them, so this is unreachable in practice.
            _ => Response::close(),
        }
    }

    fn on_connect(&mut self, c: Connect, printer: &EmulatedPrinter) -> Response {
        if !printer.authenticates(&c) {
            return Response {
                send: vec![Packet::ConnAck {
                    session_present: false,
                    code: ConnectCode::BadCredentials,
                }],
                close: true,
                ..Response::default()
            };
        }
        self.phase = Phase::Connected;
        self.client_id = c.client_id;
        self.keep_alive = c.keep_alive;
        // `session_present: false` always — we keep no persistent session, so
        // claiming otherwise would tell a reconnecting client its subscriptions
        // survived when they did not.
        Response::send(Packet::ConnAck {
            session_present: false,
            code: ConnectCode::Accepted,
        })
    }

    fn on_subscribe(
        &mut self,
        packet_id: u16,
        filters: &[(String, u8)],
        printer: &EmulatedPrinter,
    ) -> Response {
        let report = printer.report_topic();
        let codes = filters
            .iter()
            .map(|(filter, qos)| {
                // Matched as an MQTT filter, not compared as a string: a client
                // subscribing `device/+/report` means our report topic, and
                // refusing it would leave a healthy-looking connection that
                // never delivers anything.
                if topic_matches(filter, &report) {
                    self.report_filters.insert(filter.clone());
                    // Granted at the QoS the client asked for, capped at 1.
                    (*qos).min(1)
                } else {
                    // 0x80 = failure (§3.9.3). We front exactly one printer, so
                    // a filter that doesn't cover its report topic is a promise
                    // of data that is never coming.
                    0x80
                }
            })
            .collect();
        Response::send(Packet::SubAck { packet_id, codes })
    }

    fn on_publish(
        &mut self,
        p: Publish,
        printer: &EmulatedPrinter,
        cache: &UpstreamCache,
    ) -> Response {
        // QoS 1 is acknowledged whatever we decide to do with the payload: a
        // client that never gets its PUBACK retries forever.
        let mut response = match (p.qos, p.packet_id) {
            (1, Some(packet_id)) => Response::send(Packet::PubAck { packet_id }),
            _ => Response::nothing(),
        };
        // A resend of something already handled: acknowledge it again — the
        // client is waiting for that — but do not put the command through a
        // second time. Keyed on DUP as well as the id, because ids cycle and a
        // *new* command reusing one is not a retransmission.
        if let (1, Some(packet_id), true) = (p.qos, p.packet_id, p.dup)
            && self.recent_packet_ids.contains(&packet_id)
        {
            return response;
        }
        if let (1, Some(packet_id)) = (p.qos, p.packet_id) {
            self.recent_packet_ids.push_back(packet_id);
            while self.recent_packet_ids.len() > RECENT_PACKET_IDS {
                self.recent_packet_ids.pop_front();
            }
        }
        if p.topic != printer.request_topic() {
            return response;
        }
        let Ok(payload) = serde_json::from_slice::<Value>(&p.payload) else {
            // Unparseable means unclassifiable, and an unclassifiable payload
            // cannot be checked against the control policy — so it cannot go
            // through. Dropping one junk message is not worth a disconnect.
            return response;
        };

        match classify(&payload) {
            RequestKind::PushAll => {
                // `pushing.start` resumes a stream this client paused; a plain
                // `pushall` is harmless to apply the same way.
                self.push_paused = false;
                match cache.snapshot_reply() {
                    // The cache is the *merged* picture, which is strictly better
                    // than what the printer would send (deltas since whenever), and
                    // it spares the machine N clients' polling.
                    Some(reply) => response
                        .send
                        .push(Packet::Publish(report_publish(printer.serial(), &reply))),
                    // Nothing cached yet: ask the real printer rather than
                    // answer with an empty snapshot the client would believe.
                    None => response.upstream.push(payload),
                }
            }
            // Absorbed on purpose — see RequestKind::StopPushing. No reply: the
            // printer doesn't ACK this either, and the client's own view is
            // simply that reports stop mattering to it.
            RequestKind::StopPushing => self.push_paused = true,
            RequestKind::GetVersion => match cache.version_reply(sequence_id(&payload)) {
                Some(reply) => response
                    .send
                    .push(Packet::Publish(report_publish(printer.serial(), &reply))),
                None => response.upstream.push(payload),
            },
            RequestKind::Control => match printer.control() {
                ControlPolicy::Full => response.upstream.push(payload),
                ControlPolicy::ReadOnly => {
                    // Refuse the way the printer refuses. Silence would leave
                    // Bambu Studio waiting for an ACK that never comes.
                    let refusal = refusal_ack(&payload);
                    response
                        .send
                        .push(Packet::Publish(report_publish(printer.serial(), &refusal)));
                }
                ControlPolicy::Intercept => {
                    // Accepted, and answered as accepted: whatever is behind
                    // the relay really did receive it. What it did NOT do is go
                    // anywhere near `response.upstream`.
                    let ack = success_ack(&payload);
                    response
                        .send
                        .push(Packet::Publish(report_publish(printer.serial(), &ack)));
                    response.intercepted.push(payload);
                }
            },
        }
        response
    }
}

/// What a request payload asks for — reads can be served locally, control has to
/// reach the machine (and is what the read-only policy blocks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// `pushing.pushall`, or `pushing.start` — both mean "give me the state".
    PushAll,
    GetVersion,
    /// `pushing.stop`. Absorbed, never forwarded: it would stop the printer
    /// pushing to the **relay**, which is one client blinding all the others.
    /// The relay owns its push stream; a downstream client does not.
    StopPushing,
    Control,
}

/// Classify a request payload. Anything not recognised as a read is treated as
/// control — the safe direction to be wrong in.
pub fn classify(payload: &Value) -> RequestKind {
    let command = |category: &str| {
        payload
            .pointer(&format!("/{category}/command"))
            .and_then(Value::as_str)
    };
    match (command("pushing"), command("info")) {
        (Some("pushall" | "start"), _) => RequestKind::PushAll,
        (Some("stop"), _) => RequestKind::StopPushing,
        (_, Some("get_version")) => RequestKind::GetVersion,
        _ => RequestKind::Control,
    }
}

/// The single-key category of a request/report envelope (`print`, `info`, …).
fn category(payload: &Value) -> Option<&str> {
    payload.as_object()?.keys().next().map(String::as_str)
}

/// A request's `sequence_id`, whatever category it is under.
fn sequence_id(payload: &Value) -> Option<&str> {
    let category = category(payload)?;
    payload
        .pointer(&format!("/{category}/sequence_id"))
        .and_then(Value::as_str)
}

/// Build the ACK a read-only relay answers a control command with: the printer's
/// own failure shape, echoing the sequence id, with a reason that names us —
/// a client (or the human reading its log) should not have to guess.
///
/// No `command` key, because the observed ACK has none (docs/protocol.md): it is
/// `{sequence_id, param, result, reason}`. Adding one would make our refusals
/// the only ACKs on the wire shaped differently from the printer's.
fn refusal_ack(request: &Value) -> Value {
    ack(
        request,
        "fail",
        "refused by the bambu-rs emulator: relay is read-only",
    )
}

/// The ACK for a command the relay took for itself
/// ([`ControlPolicy::Intercept`]).
///
/// `success`, and honestly so from the client's side: the command was accepted
/// and acted on. Saying `fail` would be a lie in the other direction — and
/// would turn every button press into an error the operator has to dismiss.
fn success_ack(request: &Value) -> Value {
    ack(request, "success", "success")
}

/// The printer's ACK shape, filled in.
///
/// Echoes `sequence_id` and `param` and carries **no `command`**, because that
/// is the ACK observed on the A1 mini (docs/protocol.md):
/// `{sequence_id, param, result, reason}`. Under the intercept policy this is
/// the *only* ACK a client will ever see, so a shape of our own invention would
/// be the shape of the whole printer.
fn ack(request: &Value, result: &str, reason: &str) -> Value {
    let category = category(request).unwrap_or("print");
    let mut ack = Map::new();
    if let Some(seq) = sequence_id(request) {
        ack.insert("sequence_id".to_string(), Value::String(seq.to_string()));
    }
    if let Some(param) = request.pointer(&format!("/{category}/param")) {
        ack.insert("param".to_string(), param.clone());
    }
    // A `print` ACK carries no `command` — that is observed, and echoing one
    // there would be inventing a shape the machine does not send. Every other
    // category does need it: Bambu Studio sends `security.app_cert_install`,
    // gets back result/reason/sequence_id with no command, cannot match the
    // answer to its question, and retries forever while showing "Retrieving
    // printer information". The asymmetry is the printer's, not ours.
    if category != "print"
        && let Some(command) = request
            .pointer(&format!("/{category}/command"))
            .and_then(Value::as_str)
    {
        ack.insert("command".to_string(), Value::String(command.to_string()));
    }
    ack.insert("result".to_string(), Value::String(result.to_string()));
    ack.insert("reason".to_string(), Value::String(reason.to_string()));
    let mut out = Map::new();
    out.insert(category.to_string(), Value::Object(ack));
    Value::Object(out)
}

/// Point a client at the relay by correcting the address in `print.net`.
///
/// **The printer tells clients where it lives, and a relay that repeats it is
/// telling them to go round it.** `print.net.info[].ip` is a `u32` holding the
/// machine's own LAN address little-endian — `50374848` is `192.168.0.3`. Bambu
/// Studio reads it and uses it for everything except MQTT: the chamber camera
/// and the FTP upload both go to that address, however the user reached the
/// relay. Observed on real hardware, twice, on two independent paths.
///
/// It hides well. Grepping a captured report for `"192.168"` finds nothing,
/// because it is a number, and the conclusion "the relay leaks no address" is
/// then wrong for an hour of debugging.
///
/// `advertised` must be an address the *client* can reach — the relay's, not
/// `0.0.0.0`, which is a binding instruction rather than a place.
///
/// Returns whether anything changed.
pub fn rewrite_lan_address(message: &mut Value, advertised: std::net::Ipv4Addr) -> bool {
    let Some(info) = message
        .get_mut("print")
        .and_then(|p| p.get_mut("net"))
        .and_then(|n| n.get_mut("info"))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let want = u32::from_le_bytes(advertised.octets());
    let mut changed = false;
    for entry in info.iter_mut() {
        let Some(slot) = entry.get_mut("ip") else {
            continue;
        };
        // Zero means "no address on this interface"; inventing one there would
        // describe a network that does not exist. Every *other* entry is
        // rewritten, because a client reading the list may take any of them.
        match slot.as_u64() {
            Some(0) | None => continue,
            Some(current) if current == want as u64 => continue,
            Some(_) => {
                *slot = Value::from(want);
                changed = true;
            }
        }
    }
    changed
}

/// Wrap a report message as the QoS-0 publish the printer would send.
pub fn report_publish(serial: &str, message: &Value) -> Publish {
    Publish::at_most_once(report_topic(serial), message.to_string().into_bytes())
}

/// The merged view of the real printer, from which reads are answered.
///
/// Feed every upstream report through [`apply`](Self::apply). Until a full
/// `pushall` snapshot has arrived there is nothing honest to answer a read with,
/// which is what [`snapshot_reply`](Self::snapshot_reply) returning `None` means.
#[derive(Debug, Clone, Default)]
pub struct UpstreamCache {
    state: ReportState,
    seen_snapshot: bool,
    /// Set by the I/O layer when the printer has gone quiet for long enough to
    /// be considered offline. The clock lives out there; this flag is how the
    /// decision reaches the pure layer.
    stale: bool,
}

impl UpstreamCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge one upstream message into the cache — if it is actually a report.
    ///
    /// A command ACK wears the same envelope as a report (`{"print": {…}}`), so
    /// merging everything would splice `result`/`reason` and the ACKed
    /// `command` into the cached `print` object, and every synthesized snapshot
    /// after that would carry them. Only `push_status` (and the `get_version`
    /// inventory) describe the printer's state; everything else is answered
    /// traffic that is forwarded to clients but not remembered.
    ///
    /// The observed ACK (docs/protocol.md) carries **no `command` at all** —
    /// `{sequence_id, param, result, reason}` — so `result` is the tell, not a
    /// missing `command`. A delta that omits `command` is still a report.
    pub fn apply(&mut self, message: &Value) {
        if !is_status_report(message) {
            return;
        }
        if is_full_snapshot_message(message) {
            self.seen_snapshot = true;
            // Only a whole snapshot clears staleness. A single delta merged onto
            // a picture from before the outage would be served as a full
            // `pushall` describing a printer that may have rebooted or started a
            // different job in the meantime — the deltas are merged either way,
            // but nothing is handed out until the printer has described itself
            // completely again.
            self.stale = false;
        }
        self.state.apply(message.clone());
    }

    /// Declare the cached picture no longer live.
    ///
    /// The relay reconnects to a dropped printer indefinitely, and while it does
    /// the cache would happily keep answering reads — a client would see a print
    /// "running" on a machine that has been off for an hour, and an automation
    /// would fire on an hour-old temperature. Deciding *when* that has happened
    /// needs a clock, which is the I/O layer's job; this is where the answer
    /// lands. Cleared by the next report.
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// Whether a full snapshot has been seen **and** still describes a live
    /// printer — i.e. whether a read can be served from here at all.
    pub fn is_warm(&self) -> bool {
        self.seen_snapshot && !self.stale
    }

    /// The merged `push_status` to answer a `pushall` with, or `None` if no full
    /// snapshot has arrived yet.
    ///
    /// The result is marked `msg: 0` — it really is a full snapshot, and a client
    /// that saw `msg: 1` would treat it as a delta and keep waiting for the
    /// picture it already has. The `sequence_id` is left as the printer's own
    /// (a `push_status` carries the printer's counter, not an echo of the
    /// request — see docs/protocol.md).
    pub fn snapshot_reply(&self) -> Option<Value> {
        if !self.is_warm() {
            return None;
        }
        let mut print = self.state.get().get("print")?.as_object()?.clone();
        print.insert("command".to_string(), Value::String("push_status".into()));
        print.insert("msg".to_string(), Value::from(0));
        let mut out = Map::new();
        out.insert("print".to_string(), Value::Object(print));
        Some(Value::Object(out))
    }

    /// The cached `get_version` inventory, echoing `sequence_id` back the way an
    /// ACK does. `None` if we have never seen one.
    pub fn version_reply(&self, sequence_id: Option<&str>) -> Option<Value> {
        let mut info = self.state.get().get("info")?.as_object()?.clone();
        if info.get("command").and_then(Value::as_str) != Some("get_version") {
            return None;
        }
        match sequence_id {
            Some(seq) => {
                info.insert("sequence_id".to_string(), Value::String(seq.to_string()));
            }
            None => {
                info.remove("sequence_id");
            }
        }
        let mut out = Map::new();
        out.insert("info".to_string(), Value::Object(info));
        Some(Value::Object(out))
    }

    /// The merged state, for a caller that wants to read it directly.
    pub fn state(&self) -> &ReportState {
        &self.state
    }
}

/// Where one upstream report should go.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// Every subscribed client sees it (the printer's own pushes).
    Broadcast(Value),
    /// An ACK for one client's command, with its original `sequence_id` restored.
    ToClient { client: ClientId, message: Value },
}

/// Keeps two clients' `sequence_id`s apart.
///
/// Every client counts from `"1"`, so forwarding ids verbatim would let one
/// client read another's ACK as the answer to its own command — precisely the
/// confusion the printer's one-client habit used to prevent. Requests go
/// upstream under an id unique to this relay, and the ACK is rewritten back to
/// the caller's own id and delivered only to them.
#[derive(Debug, Default)]
pub struct SequenceRewriter {
    next: u64,
    outstanding: BTreeMap<String, Pending>,
}

#[derive(Debug)]
struct Pending {
    client: ClientId,
    original: String,
}

impl SequenceRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rewrite `payload`'s `sequence_id` to one unique across all clients,
    /// remembering who to give the ACK back to. A payload with no `sequence_id`
    /// is returned unchanged — there is nothing to collide, and inventing one
    /// would change what the printer sees.
    pub fn rewrite_request(&mut self, client: ClientId, payload: &Value) -> Value {
        let (Some(category), Some(original)) = (category(payload), sequence_id(payload)) else {
            return payload.clone();
        };
        // A `pushing` request is never echoed: its reply is a `push_status`
        // carrying the printer's own counter. Remapping one would allocate an
        // id nothing ever answers, leaving an entry outstanding until it is
        // evicted — and there is nothing to disambiguate anyway, since the
        // reply is broadcast to every subscriber regardless of who asked.
        if matches!(
            classify(payload),
            RequestKind::PushAll | RequestKind::StopPushing
        ) {
            return payload.clone();
        }
        // Prefixed so a relay id is never mistaken for the printer's own
        // counter, which is a small integer. Fixed width and zero-padded so the
        // BTreeMap orders these by age, which is what makes the eviction below
        // evict the *oldest*.
        //
        // The `2` prefix and the wrap keep every id below 2^31. The printer
        // echoes this back and we match on it, and whether firmware round-trips
        // an arbitrary 10-digit string untouched is unobserved — if it parses
        // the id as a 32-bit integer, a larger one could come back changed,
        // miss the outstanding map, and leave a command unacknowledged forever.
        // Staying in range costs nothing. The wrap is unreachable in practice
        // (10^8 commands) and harmless when it isn't: only MAX_OUTSTANDING ids
        // are live at a time.
        let id = format!("2{:08}", self.next % 100_000_000);
        self.next = self.next.wrapping_add(1);
        self.outstanding.insert(
            id.clone(),
            Pending {
                client,
                original: original.to_string(),
            },
        );
        // A command the printer never answers leaves its entry behind, and a
        // client that never disconnects never triggers `forget_client`. Give up
        // on the oldest rather than grow forever: its ACK, if it ever comes, is
        // broadcast instead of delivered — noise, not a hang.
        while self.outstanding.len() > MAX_OUTSTANDING {
            self.outstanding.pop_first();
        }
        let mut out = payload.clone();
        if let Some(obj) = out.get_mut(category).and_then(Value::as_object_mut) {
            obj.insert("sequence_id".to_string(), Value::String(id));
        }
        out
    }

    /// Decide where an upstream report goes: to the one client whose command it
    /// answers, or to everyone.
    pub fn route(&mut self, message: &Value) -> Route {
        let Some(category) = category(message) else {
            return Route::Broadcast(message.clone());
        };
        let Some(seq) = sequence_id(message) else {
            return Route::Broadcast(message.clone());
        };
        let Some(pending) = self.outstanding.remove(seq) else {
            // Not one of ours: the printer's own push_status counter, or an ACK
            // we already delivered. Broadcasting is the safe fallback — a client
            // that doesn't recognise the id ignores it, which is exactly what it
            // would do on a real multi-client printer.
            return Route::Broadcast(message.clone());
        };
        let mut out = message.clone();
        if let Some(obj) = out.get_mut(category).and_then(Value::as_object_mut) {
            obj.insert("sequence_id".to_string(), Value::String(pending.original));
        }
        Route::ToClient {
            client: pending.client,
            message: out,
        }
    }

    /// Drop everything still owed to a client that has gone away, so a departed
    /// client's ids can't accumulate for the life of the process.
    pub fn forget_client(&mut self, client: ClientId) {
        self.outstanding.retain(|_, p| p.client != client);
    }

    /// How many requests are still awaiting an ACK. For tests and diagnostics.
    pub fn outstanding(&self) -> usize {
        self.outstanding.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SERIAL: &str = "0309FA123456789";
    const CODE: &str = "12345678";

    fn printer() -> EmulatedPrinter {
        EmulatedPrinter::new(SERIAL, CODE)
    }

    fn connect(user: &str, pass: &str) -> Packet {
        Packet::Connect(Connect {
            client_id: "bambu-studio".into(),
            username: Some(user.into()),
            password: Some(pass.into()),
            keep_alive: 60,
            clean_session: true,
        })
    }

    /// A session that has completed the handshake, ready for the interesting part.
    fn connected() -> ClientSession {
        let mut s = ClientSession::new();
        let r = s.handle(connect("bblp", CODE), &printer(), &UpstreamCache::new());
        assert!(!r.close, "the handshake should have been accepted");
        s
    }

    // ---- handshake -------------------------------------------------------

    #[test]
    fn the_right_access_code_is_accepted() {
        let r =
            ClientSession::new().handle(connect("bblp", CODE), &printer(), &UpstreamCache::new());
        assert_eq!(
            r.send,
            vec![Packet::ConnAck {
                session_present: false,
                code: ConnectCode::Accepted
            }]
        );
        assert!(!r.close);
    }

    #[test]
    fn a_wrong_access_code_is_refused_and_the_connection_closed() {
        // The emulator is exactly as protected as the printer it fronts: the
        // access code is the only thing between a LAN peer and a machine that
        // heats and moves.
        let r = ClientSession::new().handle(
            connect("bblp", "87654321"),
            &printer(),
            &UpstreamCache::new(),
        );
        assert_eq!(
            r.send,
            vec![Packet::ConnAck {
                session_present: false,
                code: ConnectCode::BadCredentials
            }]
        );
        assert!(r.close, "a refused client must not stay connected");
        assert!(r.upstream.is_empty());
    }

    #[test]
    fn the_username_must_be_bblp_like_the_real_printer() {
        let r =
            ClientSession::new().handle(connect("admin", CODE), &printer(), &UpstreamCache::new());
        assert!(matches!(
            r.send.as_slice(),
            [Packet::ConnAck {
                code: ConnectCode::BadCredentials,
                ..
            }]
        ));
        assert!(r.close);
    }

    #[test]
    fn anything_before_connect_closes_the_connection_silently() {
        // MQTT 3.1.1 §3.1.0: the first packet MUST be CONNECT, and the only
        // legal response to a violation is to close without a reply.
        let mut s = ClientSession::new();
        let r = s.handle(Packet::PingReq, &printer(), &UpstreamCache::new());
        assert!(r.send.is_empty());
        assert!(r.close);
    }

    #[test]
    fn a_second_connect_is_a_protocol_violation() {
        let mut s = connected();
        let r = s.handle(connect("bblp", CODE), &printer(), &UpstreamCache::new());
        assert!(r.close);
        assert!(r.send.is_empty());
    }

    // ---- subscribe -------------------------------------------------------

    #[test]
    fn subscribing_to_this_printers_report_topic_is_granted() {
        let mut s = connected();
        let r = s.handle(
            Packet::Subscribe {
                packet_id: 1,
                filters: vec![(format!("device/{SERIAL}/report"), 0)],
            },
            &printer(),
            &UpstreamCache::new(),
        );
        assert_eq!(
            r.send,
            vec![Packet::SubAck {
                packet_id: 1,
                codes: vec![0]
            }]
        );
        assert!(s.wants_reports(), "the session should now receive reports");
    }

    #[test]
    fn a_wildcard_that_covers_this_printers_report_topic_is_granted() {
        // A filter comparison by string equality refuses these, and the client
        // then sits on a healthy connection receiving nothing — the worst
        // failure this can have, because everything looks fine.
        for filter in ["device/+/report", "#", "device/#"] {
            let mut s = connected();
            let r = s.handle(
                Packet::Subscribe {
                    packet_id: 1,
                    filters: vec![(filter.to_string(), 0)],
                },
                &printer(),
                &UpstreamCache::new(),
            );
            assert_eq!(
                r.send,
                vec![Packet::SubAck {
                    packet_id: 1,
                    codes: vec![0]
                }],
                "{filter} covers our report topic"
            );
            assert!(s.wants_reports(), "{filter}");
        }
    }

    #[test]
    fn subscribing_to_another_serials_topic_is_refused() {
        // We front exactly one printer. Granting a filter that doesn't cover its
        // report topic would promise data we will never deliver, and the client
        // would sit there waiting for it.
        let mut s = connected();
        let r = s.handle(
            Packet::Subscribe {
                packet_id: 2,
                filters: vec![
                    ("device/SOMEONEELSE/report".into(), 0),
                    ("device/SOMEONEELSE/#".into(), 0),
                ],
            },
            &printer(),
            &UpstreamCache::new(),
        );
        assert_eq!(
            r.send,
            vec![Packet::SubAck {
                packet_id: 2,
                codes: vec![0x80, 0x80] // failure, per §3.9.3
            }]
        );
        assert!(!s.wants_reports());
    }

    #[test]
    fn unsubscribing_stops_the_reports() {
        let mut s = connected();
        s.handle(
            Packet::Subscribe {
                packet_id: 1,
                filters: vec![(format!("device/{SERIAL}/report"), 0)],
            },
            &printer(),
            &UpstreamCache::new(),
        );
        let r = s.handle(
            Packet::Unsubscribe {
                packet_id: 5,
                filters: vec![format!("device/{SERIAL}/report")],
            },
            &printer(),
            &UpstreamCache::new(),
        );
        assert_eq!(r.send, vec![Packet::UnsubAck { packet_id: 5 }]);
        assert!(!s.wants_reports());
    }

    #[test]
    fn unsubscribing_one_of_two_overlapping_filters_keeps_the_reports_coming() {
        // MQTT subscriptions are per-filter. A client holding both the exact
        // topic and a wildcard that covers it, then dropping one, is still
        // subscribed — a single boolean would switch its reports off.
        let mut s = connected();
        for filter in [
            format!("device/{SERIAL}/report"),
            "device/+/report".to_string(),
        ] {
            s.handle(
                Packet::Subscribe {
                    packet_id: 1,
                    filters: vec![(filter, 0)],
                },
                &printer(),
                &UpstreamCache::new(),
            );
        }
        s.handle(
            Packet::Unsubscribe {
                packet_id: 2,
                filters: vec![format!("device/{SERIAL}/report")],
            },
            &printer(),
            &UpstreamCache::new(),
        );
        assert!(s.wants_reports(), "the wildcard subscription is still live");

        // Dropping the last one does stop them.
        s.handle(
            Packet::Unsubscribe {
                packet_id: 3,
                filters: vec!["device/+/report".to_string()],
            },
            &printer(),
            &UpstreamCache::new(),
        );
        assert!(!s.wants_reports());
    }

    #[test]
    fn pushing_stop_stops_this_clients_reports_without_touching_anyone_elses() {
        // The client asked to stop being pushed to. We can't forward that (it
        // would stop the printer pushing to the relay), but ignoring it means
        // the client keeps receiving what it just asked to stop. Honour it for
        // this session only.
        let mut s = connected();
        s.handle(
            Packet::Subscribe {
                packet_id: 1,
                filters: vec![(format!("device/{SERIAL}/report"), 0)],
            },
            &printer(),
            &UpstreamCache::new(),
        );
        assert!(s.wants_reports());

        let r = s.handle(
            publish_request(
                json!({"pushing": {"sequence_id": "1", "command": "stop"}}),
                None,
            ),
            &printer(),
            &cache_with_snapshot(),
        );
        assert!(r.upstream.is_empty(), "never reaches the printer");
        assert!(!s.wants_reports(), "but this client stops being fed");

        // `start` resumes it, and hands over the state it missed.
        let r = s.handle(
            publish_request(
                json!({"pushing": {"sequence_id": "2", "command": "start"}}),
                None,
            ),
            &printer(),
            &cache_with_snapshot(),
        );
        assert!(s.wants_reports());
        assert_eq!(r.send.len(), 1, "and catches it up with a snapshot");
    }

    #[test]
    fn a_wildcard_subscription_can_be_undone_by_the_same_wildcard() {
        // Whatever SUBSCRIBE accepts, UNSUBSCRIBE must undo. Matching one as a
        // filter and the other as a string would leave a client still being
        // sent reports it had asked to stop.
        let mut s = connected();
        s.handle(
            Packet::Subscribe {
                packet_id: 1,
                filters: vec![("device/+/report".into(), 0)],
            },
            &printer(),
            &UpstreamCache::new(),
        );
        assert!(s.wants_reports());
        s.handle(
            Packet::Unsubscribe {
                packet_id: 2,
                filters: vec!["device/+/report".into()],
            },
            &printer(),
            &UpstreamCache::new(),
        );
        assert!(!s.wants_reports());
    }

    // ---- requests --------------------------------------------------------

    fn publish_request(payload: serde_json::Value, packet_id: Option<u16>) -> Packet {
        Packet::Publish(Publish {
            topic: format!("device/{SERIAL}/request"),
            payload: payload.to_string().into_bytes(),
            qos: if packet_id.is_some() { 1 } else { 0 },
            packet_id,
            retain: false,
            dup: false,
        })
    }

    #[test]
    fn a_control_command_is_acked_at_qos1_and_forwarded_upstream() {
        let mut s = connected();
        let cmd = json!({"print": {"sequence_id": "3", "command": "pause"}});
        let r = s.handle(
            publish_request(cmd.clone(), Some(9)),
            &printer(),
            &UpstreamCache::new(),
        );
        assert_eq!(r.send, vec![Packet::PubAck { packet_id: 9 }]);
        assert_eq!(r.upstream, vec![cmd], "the printer must actually see it");
    }

    #[test]
    fn a_retransmitted_command_is_acked_but_not_run_twice() {
        // A client that misses its PUBACK resends the same PUBLISH with DUP set.
        // Forwarded again it becomes a second command with a fresh relay
        // sequence id — indistinguishable, at the printer, from the operator
        // asking twice. For `pause` that is harmless; for a `gcode_line` that
        // extrudes or moves, it is not.
        let mut s = connected();
        let cmd = json!({"print": {"sequence_id": "3", "command": "gcode_line", "param": "G28"}});
        let first = s.handle(
            publish_request(cmd.clone(), Some(9)),
            &printer(),
            &UpstreamCache::new(),
        );
        assert_eq!(first.upstream, vec![cmd.clone()]);

        let again = Packet::Publish(Publish {
            topic: format!("device/{SERIAL}/request"),
            payload: cmd.to_string().into_bytes(),
            qos: 1,
            packet_id: Some(9),
            retain: false,
            dup: true, // the retransmission flag
        });
        let r = s.handle(again, &printer(), &UpstreamCache::new());
        assert_eq!(
            r.send,
            vec![Packet::PubAck { packet_id: 9 }],
            "still acknowledged, or the client keeps resending"
        );
        assert!(r.upstream.is_empty(), "but the machine only hears it once");
    }

    #[test]
    fn reusing_a_packet_id_for_a_new_command_is_not_mistaken_for_a_retransmission() {
        // Packet ids cycle. Only DUP marks a resend, so a fresh command that
        // happens to reuse an id must go through.
        let mut s = connected();
        let first = json!({"print": {"sequence_id": "1", "command": "pause"}});
        s.handle(
            publish_request(first, Some(9)),
            &printer(),
            &UpstreamCache::new(),
        );

        let second = json!({"print": {"sequence_id": "2", "command": "resume"}});
        let r = s.handle(
            publish_request(second.clone(), Some(9)),
            &printer(),
            &UpstreamCache::new(),
        );
        assert_eq!(r.upstream, vec![second]);
    }

    #[test]
    fn a_qos0_request_is_forwarded_without_a_puback() {
        let mut s = connected();
        let cmd = json!({"print": {"sequence_id": "3", "command": "resume"}});
        let r = s.handle(
            publish_request(cmd.clone(), None),
            &printer(),
            &UpstreamCache::new(),
        );
        assert!(r.send.is_empty(), "QoS 0 is fire-and-forget");
        assert_eq!(r.upstream, vec![cmd]);
    }

    #[test]
    fn a_publish_to_a_foreign_topic_is_never_forwarded() {
        let mut s = connected();
        let r = s.handle(
            Packet::Publish(Publish {
                topic: "device/SOMEONEELSE/request".into(),
                payload: json!({"print": {"command": "stop"}})
                    .to_string()
                    .into_bytes(),
                qos: 1,
                packet_id: Some(4),
                retain: false,
                dup: false,
            }),
            &printer(),
            &UpstreamCache::new(),
        );
        // Still ACKed — QoS 1 requires it, and a silent hang is worse than a
        // dropped message — but nothing reaches the machine.
        assert_eq!(r.send, vec![Packet::PubAck { packet_id: 4 }]);
        assert!(r.upstream.is_empty());
    }

    #[test]
    fn a_payload_that_is_not_json_is_dropped_rather_than_relayed_blind() {
        // We classify requests to enforce the read-only policy; something we
        // cannot parse cannot be classified, so it cannot be allowed through.
        let mut s = connected();
        let r = s.handle(
            Packet::Publish(Publish {
                topic: format!("device/{SERIAL}/request"),
                payload: b"\xff\xfe not json".to_vec(),
                qos: 0,
                packet_id: None,
                retain: false,
                dup: false,
            }),
            &printer(),
            &UpstreamCache::new(),
        );
        assert!(r.upstream.is_empty());
        assert!(!r.close, "junk from one client is not fatal");
    }

    // ---- answering reads from the cache ----------------------------------

    fn cache_with_snapshot() -> UpstreamCache {
        let mut c = UpstreamCache::new();
        c.apply(&json!({"print": {
            "command": "push_status", "msg": 0, "sequence_id": "812",
            "gcode_state": "RUNNING", "mc_percent": 40, "nozzle_temper": 220.0,
        }}));
        // …then a delta, exactly as the real printer sends them.
        c.apply(&json!({"print": {"command": "push_status", "msg": 1, "mc_percent": 41}}));
        c
    }

    #[test]
    fn a_pushall_is_answered_from_the_cache_and_not_forwarded() {
        // This is the whole point of the emulator: a client that connects
        // mid-print gets the *merged* picture immediately, and the real printer
        // is spared N clients' polling.
        let mut s = connected();
        let cache = cache_with_snapshot();
        let r = s.handle(
            publish_request(
                json!({"pushing": {"sequence_id": "1", "command": "pushall"}}),
                None,
            ),
            &printer(),
            &cache,
        );
        assert!(r.upstream.is_empty(), "the cache already has the answer");
        let [Packet::Publish(p)] = r.send.as_slice() else {
            panic!("expected one report publish, got {:?}", r.send)
        };
        assert_eq!(p.topic, format!("device/{SERIAL}/report"));
        let v: serde_json::Value = serde_json::from_slice(&p.payload).unwrap();
        assert_eq!(v["print"]["command"], "push_status");
        assert_eq!(
            v["print"]["msg"], 0,
            "a synthesized pushall is a FULL snapshot"
        );
        assert_eq!(
            v["print"]["mc_percent"], 41,
            "merged, not the stale snapshot"
        );
        assert_eq!(v["print"]["gcode_state"], "RUNNING");
    }

    #[test]
    fn one_client_cannot_switch_off_everybody_elses_reports() {
        // `pushing.stop` tells the printer to stop pushing. Forwarded, it would
        // stop pushing to the RELAY — so one client quietly blinds every other
        // client and the dashboard. The relay owns its own push stream; a
        // downstream client's start/stop is about that client, and stops here.
        let mut s = connected();
        let r = s.handle(
            publish_request(
                json!({"pushing": {"sequence_id": "1", "command": "stop"}}),
                None,
            ),
            &printer(),
            &cache_with_snapshot(),
        );
        assert!(r.upstream.is_empty(), "must never reach the printer");
        assert!(r.send.is_empty(), "nothing to say about it either");
    }

    #[test]
    fn pushing_start_is_answered_like_a_pushall_rather_than_forwarded() {
        // Same reasoning: the relay is already receiving. What the client wants
        // is the state, and the cache has it.
        let mut s = connected();
        let r = s.handle(
            publish_request(
                json!({"pushing": {"sequence_id": "1", "command": "start"}}),
                None,
            ),
            &printer(),
            &cache_with_snapshot(),
        );
        assert!(r.upstream.is_empty());
        let [Packet::Publish(p)] = r.send.as_slice() else {
            panic!("expected a snapshot, got {:?}", r.send)
        };
        let v: serde_json::Value = serde_json::from_slice(&p.payload).unwrap();
        assert_eq!(v["print"]["msg"], 0);
        assert_eq!(v["print"]["gcode_state"], "RUNNING");
    }

    #[test]
    fn a_stale_cache_stops_answering_reads() {
        // The printer has been unplugged for an hour and the relay is quietly
        // reconnecting. Answering a pushall from the cache would show Studio
        // "RUNNING 41%" and fire a Home Assistant automation on an hour-old
        // temperature. Once the I/O layer declares the printer gone, the cache
        // stops pretending.
        let mut c = cache_with_snapshot();
        assert!(c.is_warm());
        c.mark_stale();
        assert!(!c.is_warm());
        assert!(c.snapshot_reply().is_none());

        let mut s = connected();
        let req = json!({"pushing": {"sequence_id": "1", "command": "pushall"}});
        let r = s.handle(publish_request(req.clone(), None), &printer(), &c);
        assert_eq!(r.upstream, vec![req], "asked upstream instead of guessing");
        assert!(r.send.is_empty(), "and answered the client nothing");
    }

    #[test]
    fn only_a_whole_snapshot_brings_a_stale_cache_back_to_life() {
        // A delta merged onto a picture from before the outage would be handed
        // out as a full `pushall` — describing a printer that may have rebooted
        // or started a different job while it was away. Merge the delta, but
        // stay quiet until the printer has described itself completely again.
        let mut c = cache_with_snapshot();
        c.mark_stale();
        c.apply(&json!({"print": {"command": "push_status", "msg": 1, "mc_percent": 50}}));
        assert!(!c.is_warm(), "one delta is not a fresh picture");
        assert!(c.snapshot_reply().is_none());

        // The reconnect's pushall is what settles it — and the delta was not
        // thrown away in the meantime.
        c.apply(&json!({"print": {
            "command": "push_status", "msg": 0, "gcode_state": "RUNNING", "mc_percent": 51,
        }}));
        assert!(c.is_warm());
        assert_eq!(c.snapshot_reply().unwrap()["print"]["mc_percent"], 51);
    }

    #[test]
    fn a_pushall_before_the_cache_is_warm_is_forwarded_instead() {
        // Nothing to answer with yet: ask the real printer rather than reply
        // with an empty snapshot the client would believe.
        let mut s = connected();
        let req = json!({"pushing": {"sequence_id": "1", "command": "pushall"}});
        let r = s.handle(
            publish_request(req.clone(), None),
            &printer(),
            &UpstreamCache::new(),
        );
        assert_eq!(r.upstream, vec![req]);
        assert!(r.send.is_empty());
    }

    #[test]
    fn a_command_ack_never_leaks_into_the_synthesized_snapshot() {
        // An ACK is `{"print": {sequence_id, command, result, reason}}` — the
        // same envelope as a report. Merged blindly it would overwrite the
        // cached `command` and glue `result: "success"` onto every snapshot we
        // then hand out as a push_status.
        let mut c = cache_with_snapshot();
        c.apply(&json!({"print": {
            "sequence_id": "9", "command": "pause", "result": "success", "reason": "success",
        }}));
        let reply = c.snapshot_reply().unwrap();
        assert_eq!(reply["print"]["command"], "push_status");
        assert!(reply["print"].get("result").is_none(), "leaked: {reply}");
        assert!(reply["print"].get("reason").is_none(), "leaked: {reply}");
        assert_eq!(reply["print"]["gcode_state"], "RUNNING", "state survives");
    }

    #[test]
    fn the_observed_ack_shape_which_carries_no_command_is_still_not_a_report() {
        // docs/protocol.md's captured gcode_line ACK has no `command` key at
        // all: {"print": {sequence_id, param, result, reason}}. A rule keyed
        // only on `command` would wave it straight into the cache.
        let mut c = cache_with_snapshot();
        c.apply(&json!({"print": {
            "sequence_id": "1", "param": "G28", "result": "success", "reason": "success",
        }}));
        let reply = c.snapshot_reply().unwrap();
        assert!(reply["print"].get("result").is_none(), "leaked: {reply}");
        assert!(reply["print"].get("param").is_none(), "leaked: {reply}");
    }

    #[test]
    fn a_delta_with_no_command_is_still_treated_as_a_report() {
        // The other side of that coin: older firmware may omit `command` on a
        // delta, and dropping those would freeze the cache mid-print.
        let mut c = cache_with_snapshot();
        c.apply(&json!({"print": {"msg": 1, "mc_percent": 55}}));
        assert_eq!(c.snapshot_reply().unwrap()["print"]["mc_percent"], 55);
    }

    #[test]
    fn an_ack_under_info_does_not_become_the_version_reply() {
        let mut c = UpstreamCache::new();
        c.apply(&json!({"info": {"sequence_id": "2", "command": "resume", "result": "success"}}));
        assert!(c.version_reply(Some("1")).is_none());
    }

    #[test]
    fn get_version_is_answered_from_the_cache_echoing_the_requests_sequence_id() {
        let mut s = connected();
        let mut cache = UpstreamCache::new();
        cache.apply(&json!({"info": {
            "command": "get_version", "sequence_id": "1",
            "module": [{"name": "ota", "sw_ver": "01.07.02.00"}],
        }}));
        let r = s.handle(
            publish_request(
                json!({"info": {"sequence_id": "77", "command": "get_version"}}),
                None,
            ),
            &printer(),
            &cache,
        );
        assert!(r.upstream.is_empty());
        let [Packet::Publish(p)] = r.send.as_slice() else {
            panic!("expected one report publish, got {:?}", r.send)
        };
        let v: serde_json::Value = serde_json::from_slice(&p.payload).unwrap();
        assert_eq!(v["info"]["command"], "get_version");
        assert_eq!(
            v["info"]["sequence_id"], "77",
            "an ACK echoes the request's id"
        );
        assert_eq!(v["info"]["module"][0]["sw_ver"], "01.07.02.00");
    }

    // ---- the read-only policy --------------------------------------------

    #[test]
    fn read_only_refuses_control_with_a_failed_ack_instead_of_silence() {
        // A dropped command leaves Bambu Studio spinning forever. Answer the way
        // the printer answers a rejected command, and say who rejected it.
        let mut s = connected();
        let ro = printer().read_only();
        let r = s.handle(
            publish_request(
                json!({"print": {"sequence_id": "3", "command": "pause"}}),
                None,
            ),
            &ro,
            &UpstreamCache::new(),
        );
        assert!(r.upstream.is_empty(), "nothing reaches the machine");
        let [Packet::Publish(p)] = r.send.as_slice() else {
            panic!("expected a refusal ACK, got {:?}", r.send)
        };
        let v: serde_json::Value = serde_json::from_slice(&p.payload).unwrap();
        assert_eq!(v["print"]["sequence_id"], "3");
        assert_eq!(v["print"]["result"], "fail");
        assert!(
            v["print"]["reason"].as_str().unwrap().contains("read-only"),
            "the reason should name the emulator's policy: {v}"
        );
    }

    // ---- the intercept policy --------------------------------------------

    /// Every shape of control a client can send, and the one thing that must be
    /// true of all of them under [`ControlPolicy::Intercept`].
    fn every_control_command() -> Vec<serde_json::Value> {
        vec![
            json!({"print": {"sequence_id": "1", "command": "gcode_line", "param": "G91\nG1 Y10"}}),
            json!({"print": {"sequence_id": "2", "command": "pause"}}),
            json!({"print": {"sequence_id": "3", "command": "stop"}}),
            json!({"print": {"sequence_id": "4", "command": "print_speed", "param": "3"}}),
            json!({"print": {"sequence_id": "5", "command": "project_file", "url": "ftp:///x"}}),
            json!({"system": {"sequence_id": "6", "command": "ledctrl", "led_mode": "on"}}),
            json!({"system": {"sequence_id": "7", "command": "reboot"}}),
            json!({"camera": {"sequence_id": "8", "command": "ipcam_timelapse"}}),
        ]
    }

    #[test]
    fn an_intercepting_relay_never_lets_a_control_command_upstream() {
        // The whole safety argument for `serve --emulate-doom`: a jog that
        // plays the game must not ALSO be a jog. Nothing about the payload —
        // its category, whether the mapping recognises it, whether it has a
        // sequence id — may get one past this.
        for payload in every_control_command() {
            let mut s = connected();
            let r = s.handle(
                publish_request(payload.clone(), None),
                &printer().intercepted(),
                &cache_with_snapshot(),
            );
            assert!(
                r.upstream.is_empty(),
                "{payload} reached the machine: {:?}",
                r.upstream
            );
            assert_eq!(r.intercepted, vec![payload.clone()], "{payload}");
        }
    }

    #[test]
    fn an_intercepted_command_is_acknowledged_the_way_the_printer_would() {
        // Silence leaves Bambu Studio spinning on a button press forever, and a
        // failure ACK makes every press an error the operator has to dismiss.
        // The command *was* carried out — as a keypress — so it succeeded.
        let mut s = connected();
        let r = s.handle(
            publish_request(
                json!({"print": {"sequence_id": "9", "command": "gcode_line", "param": "G28"}}),
                None,
            ),
            &printer().intercepted(),
            &cache_with_snapshot(),
        );
        let [Packet::Publish(p)] = r.send.as_slice() else {
            panic!("expected one ACK, got {:?}", r.send)
        };
        let v: serde_json::Value = serde_json::from_slice(&p.payload).unwrap();
        assert_eq!(v["print"]["sequence_id"], "9");
        assert_eq!(v["print"]["result"], "success");
        // The observed ACK echoes the param and carries no `command` — this one
        // is the only ACK a client will see, so it has to be the same shape.
        assert_eq!(v["print"]["param"], "G28");
        assert!(v["print"].get("command").is_none(), "{v}");
    }

    #[test]
    fn an_intercepted_command_is_acked_under_its_own_category() {
        // `system.ledctrl` is ACKed under /system, not /print (docs/protocol.md).
        // A client watching the wrong category never sees its answer.
        let mut s = connected();
        let r = s.handle(
            publish_request(
                json!({"system": {"sequence_id": "4", "command": "ledctrl", "led_mode": "on"}}),
                None,
            ),
            &printer().intercepted(),
            &cache_with_snapshot(),
        );
        let [Packet::Publish(p)] = r.send.as_slice() else {
            panic!("expected one ACK, got {:?}", r.send)
        };
        let v: serde_json::Value = serde_json::from_slice(&p.payload).unwrap();
        assert_eq!(v["system"]["sequence_id"], "4");
        assert_eq!(v["system"]["result"], "success");
    }

    #[test]
    fn an_intercepting_relay_still_answers_reads_from_the_cache() {
        // The client has to see a printer, or it shows nothing to press.
        let mut s = connected();
        let r = s.handle(
            publish_request(
                json!({"pushing": {"sequence_id": "1", "command": "pushall"}}),
                None,
            ),
            &printer().intercepted(),
            &cache_with_snapshot(),
        );
        assert_eq!(r.send.len(), 1, "a pushall is a read; it is always served");
        assert!(r.upstream.is_empty());
        assert!(r.intercepted.is_empty(), "a read is not a button press");
    }

    #[test]
    fn nothing_is_intercepted_under_the_ordinary_policies() {
        // The field exists for one mode; every other mode must leave it empty,
        // or a sink wired up for a game would be fed a live printer's traffic.
        let cmd = json!({"print": {"sequence_id": "1", "command": "pause"}});
        for p in [printer(), printer().read_only()] {
            let mut s = connected();
            let r = s.handle(
                publish_request(cmd.clone(), None),
                &p,
                &UpstreamCache::new(),
            );
            assert!(r.intercepted.is_empty(), "{:?}", p.control());
        }
    }

    #[test]
    fn read_only_still_answers_reads() {
        let mut s = connected();
        let r = s.handle(
            publish_request(
                json!({"pushing": {"sequence_id": "1", "command": "pushall"}}),
                None,
            ),
            &printer().read_only(),
            &cache_with_snapshot(),
        );
        assert_eq!(r.send.len(), 1, "a pushall is a read; it is always served");
        assert!(r.upstream.is_empty());
    }

    // ---- keepalive / teardown --------------------------------------------

    #[test]
    fn ping_is_answered_so_the_client_does_not_time_out() {
        let mut s = connected();
        let r = s.handle(Packet::PingReq, &printer(), &UpstreamCache::new());
        assert_eq!(r.send, vec![Packet::PingResp]);
        assert!(!r.close);
    }

    #[test]
    fn disconnect_closes_without_a_reply() {
        let mut s = connected();
        let r = s.handle(Packet::Disconnect, &printer(), &UpstreamCache::new());
        assert!(r.send.is_empty());
        assert!(r.close);
    }

    // ---- fan-out ---------------------------------------------------------

    #[test]
    fn a_report_becomes_a_publish_on_this_printers_report_topic() {
        let msg = json!({"print": {"command": "push_status", "msg": 1, "mc_percent": 7}});
        let p = report_publish(SERIAL, &msg);
        assert_eq!(p.topic, format!("device/{SERIAL}/report"));
        assert_eq!(p.qos, 0, "the printer pushes reports at QoS 0");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&p.payload).unwrap(),
            msg
        );
    }

    // ---- sequence-id remapping -------------------------------------------

    #[test]
    fn two_clients_reusing_the_same_sequence_id_get_their_own_acks_back() {
        // Every client counts from "1". Forwarded verbatim, client A would read
        // B's ACK as the answer to its own command — the exact confusion the
        // one-connection limit used to prevent.
        let mut seq = SequenceRewriter::new();
        let a = seq.rewrite_request(
            1,
            &json!({"print": {"sequence_id": "1", "command": "pause"}}),
        );
        let b = seq.rewrite_request(
            2,
            &json!({"print": {"sequence_id": "1", "command": "resume"}}),
        );
        let a_id = a["print"]["sequence_id"].as_str().unwrap().to_string();
        let b_id = b["print"]["sequence_id"].as_str().unwrap().to_string();
        assert_ne!(a_id, b_id, "forwarded ids must be unique across clients");

        // The printer ACKs B's command first.
        let ack = json!({"print": {"sequence_id": b_id, "command": "resume", "result": "success"}});
        match seq.route(&ack) {
            Route::ToClient { client, message } => {
                assert_eq!(client, 2);
                assert_eq!(
                    message["print"]["sequence_id"], "1",
                    "restored to B's own id"
                );
            }
            other => panic!("an ACK belongs to one client, got {other:?}"),
        }
        // A's is still outstanding and still routes to A.
        let ack_a = json!({"print": {"sequence_id": a_id, "result": "success"}});
        assert!(matches!(
            seq.route(&ack_a),
            Route::ToClient { client: 1, .. }
        ));
    }

    #[test]
    fn a_forwarded_sequence_id_still_fits_in_32_bits() {
        // The printer echoes this back and we match on it. Whether firmware
        // round-trips an arbitrary 10-digit string untouched is *unobserved* —
        // if it parses the id into a 32-bit integer internally, anything above
        // 2^31 could come back changed, miss the outstanding map, and leave the
        // client's command unacknowledged forever. Staying inside the range
        // costs nothing, so don't take the bet.
        let mut seq = SequenceRewriter::new();
        for _ in 0..64 {
            let fwd = seq.rewrite_request(
                1,
                &json!({"print": {"sequence_id": "1", "command": "pause"}}),
            );
            let id = fwd["print"]["sequence_id"].as_str().unwrap();
            let n: u64 = id.parse().expect("ids stay numeric");
            assert!(
                n < i32::MAX as u64,
                "{id} would overflow a signed 32-bit id"
            );
        }
    }

    #[test]
    fn the_address_a_client_is_sent_to_becomes_the_relays() {
        use std::net::Ipv4Addr;
        // The shape a real A1 mini sends. 50374848 is 192.168.0.3 read
        // little-endian — which is why grepping the JSON for "192.168" finds
        // nothing and concludes, wrongly, that no address is being relayed.
        let mut report = serde_json::json!({"print": {
            "command": "push_status",
            "net": {"conf": 0, "info": [{"ip": 50374848u32, "mask": 65535}]},
        }});
        assert!(rewrite_lan_address(
            &mut report,
            Ipv4Addr::new(192, 168, 0, 25)
        ));
        let got = report["print"]["net"]["info"][0]["ip"].as_u64().unwrap() as u32;
        assert_eq!(
            Ipv4Addr::from(got.to_le_bytes()),
            Ipv4Addr::new(192, 168, 0, 25)
        );
        // Everything else about the network is the printer's to describe.
        assert_eq!(report["print"]["net"]["info"][0]["mask"], 65535);
        assert_eq!(report["print"]["net"]["conf"], 0);
        // Idempotent.
        assert!(!rewrite_lan_address(
            &mut report,
            Ipv4Addr::new(192, 168, 0, 25)
        ));
    }

    #[test]
    fn every_interface_is_corrected_and_an_empty_one_is_left_empty() {
        use std::net::Ipv4Addr;
        // A client may take any entry in the list, so correcting only the first
        // would leave a working route to the printer in the second. A zero is
        // "no address here" — filling it in would describe a network that does
        // not exist.
        let mut report = serde_json::json!({"print": {"net": {"info": [
            {"ip": 50374848u32}, {"ip": 0}, {"ip": 16843009u32},
        ]}}});
        assert!(rewrite_lan_address(&mut report, Ipv4Addr::new(10, 0, 0, 7)));
        let want = u32::from_le_bytes(Ipv4Addr::new(10, 0, 0, 7).octets()) as u64;
        let info = report["print"]["net"]["info"].as_array().unwrap();
        assert_eq!(info[0]["ip"].as_u64(), Some(want));
        assert_eq!(info[1]["ip"].as_u64(), Some(0), "an empty slot stays empty");
        assert_eq!(info[2]["ip"].as_u64(), Some(want));
    }

    #[test]
    fn a_message_with_no_network_section_is_untouched() {
        use std::net::Ipv4Addr;
        // Deltas mostly carry temperatures; an ACK carries none of this.
        let mut delta = serde_json::json!({"print": {"mc_percent": 41}});
        let before = delta.clone();
        assert!(!rewrite_lan_address(&mut delta, Ipv4Addr::new(10, 0, 0, 7)));
        assert_eq!(delta, before);
    }

    #[test]
    fn the_printers_own_reports_are_broadcast_untouched() {
        // push_status carries the printer's *own* counter, which must not be
        // mistaken for one of our allocated ids.
        let mut seq = SequenceRewriter::new();
        seq.rewrite_request(
            1,
            &json!({"print": {"sequence_id": "1", "command": "pause"}}),
        );
        let report = json!({"print": {"command": "push_status", "msg": 1, "sequence_id": "812"}});
        match seq.route(&report) {
            Route::Broadcast(m) => assert_eq!(m, report),
            other => panic!("a status report goes to everyone, got {other:?}"),
        }
    }

    #[test]
    fn a_pushall_is_forwarded_unchanged_and_leaves_nothing_outstanding() {
        // A pushall's reply is a `push_status` carrying the printer's OWN
        // counter, not an echo — so a rewritten id would never come back, and
        // the mapping would sit in the outstanding map until evicted. Nothing
        // to disambiguate either: the reply goes to everyone regardless.
        let mut seq = SequenceRewriter::new();
        let req = json!({"pushing": {"sequence_id": "1", "command": "pushall"}});
        assert_eq!(seq.rewrite_request(1, &req), req);
        assert_eq!(seq.outstanding(), 0);
    }

    #[test]
    fn a_request_without_a_sequence_id_is_forwarded_unchanged() {
        let mut seq = SequenceRewriter::new();
        let req = json!({"pushing": {"command": "pushall"}});
        assert_eq!(seq.rewrite_request(1, &req), req);
    }

    #[test]
    fn a_delivered_ack_stops_being_outstanding() {
        // Otherwise the map grows for the life of the process, and a recycled
        // id would route to a long-gone client.
        let mut seq = SequenceRewriter::new();
        let fwd = seq.rewrite_request(
            1,
            &json!({"print": {"sequence_id": "1", "command": "pause"}}),
        );
        let id = fwd["print"]["sequence_id"].as_str().unwrap().to_string();
        let ack = json!({"print": {"sequence_id": id, "result": "success"}});
        assert!(matches!(seq.route(&ack), Route::ToClient { .. }));
        assert_eq!(seq.outstanding(), 0);
        // A repeat of the same ACK is nobody's now; broadcasting is the safe
        // fallback (a client that doesn't recognise the id ignores it).
        assert!(matches!(seq.route(&ack), Route::Broadcast(_)));
    }

    #[test]
    fn outstanding_requests_cannot_pile_up_without_bound() {
        // A command the printer never ACKs leaves its entry behind, and
        // `forget_client` only fires when the client goes away. A dashboard left
        // open for a month must not turn that into a slow leak.
        let mut seq = SequenceRewriter::new();
        let mut ids = Vec::new();
        for i in 0..(MAX_OUTSTANDING + 50) {
            let fwd = seq.rewrite_request(
                1,
                &json!({"print": {"sequence_id": i.to_string(), "command": "pause"}}),
            );
            ids.push(fwd["print"]["sequence_id"].as_str().unwrap().to_string());
        }
        assert_eq!(seq.outstanding(), MAX_OUTSTANDING);

        // The newest survive; the oldest are the ones given up on.
        let newest = json!({"print": {"sequence_id": ids.last().unwrap(), "result": "success"}});
        assert!(matches!(seq.route(&newest), Route::ToClient { .. }));
        let oldest = json!({"print": {"sequence_id": &ids[0], "result": "success"}});
        assert!(matches!(seq.route(&oldest), Route::Broadcast(_)));
    }

    #[test]
    fn a_departing_clients_outstanding_requests_are_forgotten() {
        let mut seq = SequenceRewriter::new();
        seq.rewrite_request(
            1,
            &json!({"print": {"sequence_id": "1", "command": "pause"}}),
        );
        seq.rewrite_request(
            2,
            &json!({"print": {"sequence_id": "1", "command": "pause"}}),
        );
        seq.forget_client(1);
        assert_eq!(seq.outstanding(), 1);
    }
}
