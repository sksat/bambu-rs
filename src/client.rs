//! LAN MQTT client — the I/O layer.
//!
//! One-shot, stateless cycle (connect → `pushall` → collect snapshot →
//! disconnect), which suits a CLI invocation and respects the A1/P1 single-MQTT-
//! connection limit. Built on `rumqttc` + `rustls`; the printer presents a
//! self-signed certificate with no CA chain, so we accept any certificate while
//! still validating the TLS handshake signatures.
//!
//! [`StatusSource`] abstracts snapshot fetching so consumers (and tests) don't
//! depend on the concrete MQTT client.

use std::time::Duration;

use rumqttc::{
    AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS, TlsConfiguration, Transport,
};
use serde_json::Value;

use crate::config::ResolvedTarget;
use crate::core::command::{Command, SequenceIds};
use crate::core::report::{ReportState, is_full_snapshot_message};
use crate::core::session::VerifySession;
use crate::core::settle::{self, Settle, SettleStep};

// The wait's verdict is pure data, so it lives in `core`; re-exported here
// because callers reach it through `send_sequence`.
pub use crate::core::settle::SettleOutcome;
use crate::core::status::GcodeState;
use crate::core::version::DeviceVersion;

// Verify-result types live in `core` (pure, I/O-free) and are re-exported here so
// existing `client::{CommandOutcome, VerifyStage}` users keep working.
pub use crate::core::session::{CommandOutcome, VerifyStage};

const MQTT_PORT: u16 = 8883;
const MQTT_USER: &str = "bblp";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Backoff between reconnect attempts for a continuous (`reconnect`) watch.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Errors from the I/O client. Messages never include the access code.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error("MQTT error: {0}")]
    Mqtt(String),
    #[error("timed out after {0:?} (no snapshot, ACK, or terminal state in time)")]
    Timeout(Duration),
    #[error("async runtime error: {0}")]
    Runtime(String),
}

/// Something that can produce a printer status snapshot. Abstracted so the CLI
/// and tests don't depend on the concrete MQTT client.
pub trait StatusSource {
    fn fetch_snapshot(&self) -> Result<ReportState, ClientError>;
}

/// Whether [`LanMqttClient::watch`] should keep watching or stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchStep {
    Continue,
    Stop,
}

/// The result of a [`LanMqttClient::send_sequence`] run.
pub struct SequenceRun {
    /// One verdict per step **that got one** — short when the run stopped early.
    pub outcomes: Vec<CommandOutcome>,
    /// How the post-sequence wait went, or `None` if none was asked for.
    pub settled: Option<SettleOutcome>,
}

/// The printer's current activity code from a merged report, if it has one.
fn stg_cur(state: &ReportState) -> Option<i64> {
    state.pointer("/print/stg_cur").and_then(Value::as_i64)
}

/// The printer's coarse job state from a merged report, if it has one.
fn gcode_state(state: &ReportState) -> Option<GcodeState> {
    state
        .pointer("/print/gcode_state")
        .and_then(Value::as_str)
        .map(GcodeState::parse)
}

/// The printer's fault code from a merged report; `0` and absent both mean none.
fn print_error(state: &ReportState) -> Option<i64> {
    state
        .pointer("/print/print_error")
        .and_then(Value::as_i64)
        .filter(|e| *e != 0)
}

/// A per-connection-unique MQTT client id, `bambu-rs-<pid>-<n>`.
///
/// MQTT brokers normally disconnect an existing client when a new one connects
/// with the **same** client id, so a fixed id would make two concurrent bambu-rs
/// connections (e.g. `job start --watch` + `timelapse capture`) fight. The pid
/// distinguishes processes; the atomic counter distinguishes connections within a
/// process. (Observed: this A1 mini's broker happens *not* to enforce client-id
/// uniqueness or a 1-connection limit — two connections coexist — but a unique id
/// is the correct, portable behaviour regardless. See `docs/protocol.md`.)
fn unique_client_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "bambu-rs-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

// Topic naming is a protocol fact, so it lives in `core::command` alongside the
// request envelopes, and is re-exported here where every caller already looks
// for it.
pub use crate::core::command::{report_topic, request_topic};

/// A one-shot LAN MQTT client.
pub struct LanMqttClient {
    target: ResolvedTarget,
    timeout: Duration,
}

impl LanMqttClient {
    pub fn new(target: ResolvedTarget) -> Self {
        Self {
            target,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Connect, subscribe to the report topic, and request a `pushall`.
    async fn connect(&self) -> Result<(AsyncClient, EventLoop), ClientError> {
        let mut opts = MqttOptions::new(unique_client_id(), &self.target.ip, MQTT_PORT);
        opts.set_credentials(MQTT_USER, &self.target.access_code);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_transport(Transport::Tls(tls_config()?));

        let (client, eventloop) = AsyncClient::new(opts, 16);
        client
            .subscribe(report_topic(&self.target.serial), QoS::AtMostOnce)
            .await
            .map_err(|e| ClientError::Mqtt(e.to_string()))?;
        client
            .publish(
                request_topic(&self.target.serial),
                QoS::AtMostOnce,
                false,
                Command::PushAll.to_payload("0").to_string(),
            )
            .await
            .map_err(|e| ClientError::Mqtt(e.to_string()))?;
        Ok((client, eventloop))
    }

    async fn fetch_async(&self) -> Result<ReportState, ClientError> {
        // Hold `_client` so the event loop stays connected.
        let (_client, mut eventloop) = self.connect().await?;
        let mut state = ReportState::new();
        loop {
            if let Event::Incoming(Packet::Publish(p)) = poll(&mut eventloop).await?
                && let Ok(json) = serde_json::from_slice::<Value>(&p.payload)
            {
                // Wait for the actual pushall response (push_status, msg == 0),
                // not an unsolicited delta that merely carries a `print` object: a
                // delta would be a partial snapshot missing most fields. Check the
                // raw message before merging (msg is per-message).
                let full = is_full_snapshot_message(&json);
                state.apply(json);
                if full {
                    return Ok(state);
                }
            }
        }
    }

    async fn fetch_version_async(&self) -> Result<DeviceVersion, ClientError> {
        // Hold `_client` so the event loop stays connected.
        let (client, mut eventloop) = self.connect().await?;
        // connect() used sequence id "0" for the pushall; get_version gets "1".
        client
            .publish(
                request_topic(&self.target.serial),
                QoS::AtLeastOnce,
                false,
                Command::GetVersion.to_payload("1").to_string(),
            )
            .await
            .map_err(|e| ClientError::Mqtt(e.to_string()))?;

        let mut state = ReportState::new();
        loop {
            if let Event::Incoming(Packet::Publish(p)) = poll(&mut eventloop).await?
                && let Ok(json) = serde_json::from_slice::<Value>(&p.payload)
            {
                state.apply(json);
                // The get_version response arrives under `/info` on the same
                // report topic; wait for it (not the pushall that connect sent).
                if let Some(info) = state.pointer("/info")
                    && info.get("command").and_then(Value::as_str) == Some("get_version")
                {
                    return Ok(DeviceVersion::from_info(info));
                }
            }
        }
    }

    /// Fetch the printer's module/firmware inventory (`info.get_version`).
    pub fn fetch_version(&self) -> Result<DeviceVersion, ClientError> {
        self.run_with_timeout(self.fetch_version_async())
    }

    async fn watch_async<F: FnMut(&ReportState) -> WatchStep>(
        &self,
        interval: Option<Duration>,
        reconnect: bool,
        stall: Option<Duration>,
        mut on_update: F,
    ) -> Result<ReportState, ClientError> {
        // Merged state persists across reconnects so a continuous monitor keeps
        // a coherent picture through a printer reboot / Wi-Fi blip.
        let mut state = ReportState::new();
        // Stall deadline (continuous monitor only): give up if no report arrives
        // within `stall`, but reset it on every report — so a responsive printer
        // is watched indefinitely while a truly-gone one is dropped after the
        // window (reconnect attempts do NOT reset it).
        let mut deadline = stall.map(|d| tokio::time::Instant::now() + d);
        let stalled =
            |dl: Option<tokio::time::Instant>| dl.is_some_and(|d| tokio::time::Instant::now() >= d);

        'reconnect: loop {
            let (client, mut eventloop) = match self.connect().await {
                Ok(c) => c,
                Err(e) => {
                    if reconnect && !stalled(deadline) {
                        tokio::time::sleep(RECONNECT_DELAY).await;
                        continue 'reconnect;
                    }
                    if reconnect {
                        return Ok(state); // stalled out while reconnecting
                    }
                    return Err(e);
                }
            };

            // The printer's autonomous push is slow (~2s, small deltas). With an
            // interval set, poll it like Bambu Studio does — send a periodic
            // `pushall` to pull full snapshots (~1/s; the printer caps pushall
            // there) for a higher data-acquisition rate.
            let mut ticker = interval.map(|d| {
                let mut t = tokio::time::interval(d);
                t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                t
            });
            // connect() already sent the first pushall; drop the immediate tick.
            if let Some(t) = ticker.as_mut() {
                t.tick().await;
            }

            loop {
                // One step: a report (Some(Ok)), a connection error (Some(Err)),
                // or a ticker fire that sent a pushall and yielded no data (None).
                let step = async {
                    match ticker.as_mut() {
                        Some(t) => tokio::select! {
                            ev = poll(&mut eventloop) => Some(ev),
                            _ = t.tick() => {
                                let _ = client
                                    .publish(
                                        request_topic(&self.target.serial),
                                        QoS::AtMostOnce,
                                        false,
                                        Command::PushAll.to_payload("0").to_string(),
                                    )
                                    .await;
                                None
                            }
                        },
                        None => Some(poll(&mut eventloop).await),
                    }
                };
                let polled = match deadline {
                    Some(dl) => match tokio::time::timeout_at(dl, step).await {
                        Ok(v) => v,
                        Err(_) => return Ok(state), // no report within the stall window
                    },
                    None => step.await,
                };
                let ev = match polled {
                    None => continue, // ticker fired (sent pushall), not data
                    Some(Ok(ev)) => ev,
                    Some(Err(e)) => {
                        if reconnect && !stalled(deadline) {
                            tokio::time::sleep(RECONNECT_DELAY).await;
                            continue 'reconnect;
                        }
                        if reconnect {
                            return Ok(state);
                        }
                        return Err(e);
                    }
                };
                if let Event::Incoming(Packet::Publish(p)) = ev
                    && let Ok(json) = serde_json::from_slice::<Value>(&p.payload)
                {
                    state.apply(json);
                    deadline = stall.map(|d| tokio::time::Instant::now() + d); // responsive: reset
                    if state.pointer("/print").is_some()
                        && matches!(on_update(&state), WatchStep::Stop)
                    {
                        return Ok(state);
                    }
                }
            }
        }
    }

    /// Watch a job **to completion**: invoke `on_update` on every merged report
    /// until it returns [`WatchStep::Stop`], or the total `timeout` elapses
    /// (fail-fast — a dropped connection errors). With `interval` set, also send
    /// a periodic `pushall` to raise the data rate. For `job start --watch`.
    pub fn watch<F: FnMut(&ReportState) -> WatchStep>(
        &self,
        interval: Option<Duration>,
        on_update: F,
    ) -> Result<ReportState, ClientError> {
        self.run_with_timeout(self.watch_async(interval, false, None, on_update))
    }

    /// Continuously monitor: like [`watch`](Self::watch) but **never stops on
    /// its own** and **auto-reconnects** through drops. `timeout` is a *stall*
    /// window — it ends only after no report arrives for that long (reset on
    /// every report), so a responsive printer is watched indefinitely while a
    /// truly-gone one is dropped after the window. For `status --watch`.
    pub fn monitor<F: FnMut(&ReportState) -> WatchStep>(
        &self,
        interval: Option<Duration>,
        on_update: F,
    ) -> Result<ReportState, ClientError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        rt.block_on(self.watch_async(interval, true, Some(self.timeout), on_update))
    }

    /// Drive **one long-lived connection** that streams reports out and
    /// publishes requests in, for as long as both hold up.
    ///
    /// This is the connection [`bambu serve --emulate`](crate::server::emulate)
    /// is built on, and the reason it exists: every other method here is
    /// connect-per-op, so N clients relayed through N calls would be N
    /// connections to a printer that would rather have one. Here the printer
    /// sees a single client no matter how many are downstream.
    ///
    /// Every report message is handed to `on_report` raw — merging is the
    /// caller's business, because a relay has to forward the deltas *and* keep
    /// the merged picture, and only the caller knows which it wants. Payloads
    /// arriving on `requests` are published to the request topic at QoS 1,
    /// verbatim: allocating `sequence_id`s belongs to whoever is multiplexing.
    ///
    /// Returns `Ok(())` when `requests` closes (the caller is shutting down)
    /// and `Err` when the connection breaks — reconnecting is the caller's
    /// call, since only it knows whether the relay should outlive the printer
    /// being power-cycled. A request published into a connection that is about
    /// to break is lost; there is no queue behind it.
    ///
    /// **Requests are published with `try_publish`, not `publish`.** rumqttc's
    /// request channel is bounded (16 here) and is drained *only* by
    /// `eventloop.poll()` — which is the very thing this loop is not doing
    /// while it awaits a publish. `publish().await` on a full channel would
    /// therefore wait for a drain that cannot happen: not an error, not a
    /// timeout, just a link that stops relaying for every downstream client at
    /// once and never reconnects, because it never returns `Err`. A full
    /// channel means the printer is not keeping up, so the honest answer is to
    /// drop the request and say so — the same thing the layer above does, and
    /// the client's own ACK timeout is what tells it.
    #[cfg(feature = "relay")]
    pub async fn relay<F: FnMut(&Value)>(
        &self,
        interval: Option<Duration>,
        requests: &mut tokio::sync::mpsc::Receiver<Value>,
        mut on_report: F,
    ) -> Result<(), ClientError> {
        enum Step {
            Report(Result<Event, ClientError>),
            Request(Option<Value>),
            Tick,
        }

        let (client, mut eventloop) = self.connect().await?;
        let mut ticker = interval.map(|d| {
            let mut t = tokio::time::interval(d);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            t
        });
        // connect() already sent the first pushall; drop the immediate tick.
        if let Some(t) = ticker.as_mut() {
            t.tick().await;
        }

        loop {
            let step = match ticker.as_mut() {
                Some(t) => tokio::select! {
                    ev = poll(&mut eventloop) => Step::Report(ev),
                    req = requests.recv() => Step::Request(req),
                    _ = t.tick() => Step::Tick,
                },
                None => tokio::select! {
                    ev = poll(&mut eventloop) => Step::Report(ev),
                    req = requests.recv() => Step::Request(req),
                },
            };
            match step {
                Step::Report(ev) => {
                    if let Event::Incoming(Packet::Publish(p)) = ev?
                        && let Ok(json) = serde_json::from_slice::<Value>(&p.payload)
                    {
                        on_report(&json);
                    }
                }
                Step::Request(None) => return Ok(()),
                Step::Request(Some(payload)) => {
                    // Non-blocking on purpose — see the doc comment.
                    if let Err(e) = client.try_publish(
                        request_topic(&self.target.serial),
                        QoS::AtLeastOnce, // control commands go at QoS 1
                        false,
                        payload.to_string(),
                    ) {
                        eprintln!("relay: the printer is not keeping up; dropped a request: {e}");
                    }
                }
                Step::Tick => {
                    let _ = client.try_publish(
                        request_topic(&self.target.serial),
                        QoS::AtMostOnce,
                        false,
                        Command::PushAll.to_payload("0").to_string(),
                    );
                }
            }
        }
    }

    async fn send_and_watch_async<F: FnMut(&ReportState) -> WatchStep>(
        &self,
        commands: &[Command],
        mut on_update: F,
    ) -> Result<ReportState, ClientError> {
        let (client, mut eventloop) = self.connect().await?;
        // connect() already used sequence id "0" for the pushall.
        let mut ids = SequenceIds::new();
        let _ = ids.next_id();
        for cmd in commands {
            client
                .publish(
                    request_topic(&self.target.serial),
                    QoS::AtLeastOnce, // control commands go at QoS 1
                    false,
                    cmd.to_payload(&ids.next_id()).to_string(),
                )
                .await
                .map_err(|e| ClientError::Mqtt(e.to_string()))?;
        }

        let mut state = ReportState::new();
        loop {
            if let Event::Incoming(Packet::Publish(p)) = poll(&mut eventloop).await?
                && let Ok(json) = serde_json::from_slice::<Value>(&p.payload)
            {
                state.apply(json);
                if state.pointer("/print").is_some() && matches!(on_update(&state), WatchStep::Stop)
                {
                    return Ok(state);
                }
            }
        }
    }

    /// Publish `commands` (after the initial pushall) on a **single** connection,
    /// then watch the resulting reports until `on_update` stops or the timeout
    /// elapses. One connection respects the A1/P1 single-client MQTT limit.
    pub fn send_and_watch<F: FnMut(&ReportState) -> WatchStep>(
        &self,
        commands: &[Command],
        on_update: F,
    ) -> Result<ReportState, ClientError> {
        self.run_with_timeout(self.send_and_watch_async(commands, on_update))
    }

    async fn send_and_verify_async(&self, cmd: &Command) -> Result<CommandOutcome, ClientError> {
        let (client, mut eventloop) = self.connect().await?;
        // connect() used sequence id "0" for the pushall; this command gets "1".
        let seq = "1";
        client
            .publish(
                request_topic(&self.target.serial),
                QoS::AtLeastOnce,
                false,
                cmd.to_payload(seq).to_string(),
            )
            .await
            .map_err(|e| ClientError::Mqtt(e.to_string()))?;

        // All verify logic lives in the I/O-free VerifySession (see core::session,
        // unit-tested via FakePrinter). This is just the transport: feed it each
        // report message; on timeout, ask it for the unverified verdict.
        //
        // The per-phase budget starts after connect so verification gets the full
        // configured timeout regardless of how long connecting took; the outer net
        // in send_and_verify guards a connect/network hang.
        let mut session = VerifySession::new(cmd.clone(), seq);
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let ev = match tokio::time::timeout_at(deadline, poll(&mut eventloop)).await {
                Err(_) => return Ok(session.timed_out()),
                Ok(ev) => ev?,
            };
            if let Event::Incoming(Packet::Publish(p)) = ev
                && let Ok(json) = serde_json::from_slice::<Value>(&p.payload)
                && let Some(outcome) = session.observe(json)
            {
                return Ok(outcome);
            }
        }
    }

    /// Send a control command and verify it. The ACK (echoed `sequence_id` +
    /// `result`) is necessary but not sufficient: for commands with an
    /// observable effect we also confirm the effect in the report and watch for
    /// a new `print_error` (see [`CommandOutcome`]). A verify timeout yields
    /// [`CommandOutcome::Unverified`] — published but not confirmed — never
    /// assume success.
    pub fn send_and_verify(&self, cmd: &Command) -> Result<CommandOutcome, ClientError> {
        // send_and_verify_async manages its own per-phase deadline and returns
        // Unverified on a verify timeout; this outer net only guards a
        // connect/network hang.
        let net = self.timeout + Duration::from_secs(5);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        rt.block_on(async {
            tokio::time::timeout(net, self.send_and_verify_async(cmd))
                .await
                .unwrap_or(Err(ClientError::Timeout(net)))
        })
    }

    /// Connect and wait until the broker accepts the connection, bounded by
    /// `timeout`.
    ///
    /// [`connect`](Self::connect) only queues the subscribe and the pushall —
    /// the TCP/TLS handshake happens on the first `poll`, so an unreachable
    /// printer shows up as a poll that never returns, not as a failing
    /// `connect`. Waiting for the CONNACK here is what actually bounds it, and
    /// it keeps the connect cost out of the first step's verify budget.
    async fn connect_ready(&self) -> Result<(AsyncClient, EventLoop), ClientError> {
        tokio::time::timeout(self.timeout, async {
            let (client, mut eventloop) = self.connect().await?;
            loop {
                if matches!(
                    poll(&mut eventloop).await?,
                    Event::Incoming(Packet::ConnAck(_))
                ) {
                    return Ok((client, eventloop));
                }
            }
        })
        .await
        .unwrap_or(Err(ClientError::Timeout(self.timeout)))
    }

    /// Publish one command and wait for its verdict, folding every report seen
    /// on the way into `state` so a later phase can read the printer's fields.
    async fn publish_and_verify(
        &self,
        client: &AsyncClient,
        eventloop: &mut EventLoop,
        seq: &str,
        cmd: &Command,
        state: &mut ReportState,
    ) -> Result<CommandOutcome, ClientError> {
        client
            .publish(
                request_topic(&self.target.serial),
                QoS::AtLeastOnce, // control commands go at QoS 1
                false,
                cmd.to_payload(seq).to_string(),
            )
            .await
            .map_err(|e| ClientError::Mqtt(e.to_string()))?;

        // Each step gets its own budget. A sequence's own dwells (`G4 S3`)
        // make it minutes long, so one deadline for the whole run would
        // abort a healthy sequence; what needs bounding is a single step
        // going unanswered.
        let mut session = VerifySession::new(cmd.clone(), seq);
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let ev = match tokio::time::timeout_at(deadline, poll(eventloop)).await {
                Err(_) => return Ok(session.timed_out()),
                Ok(ev) => ev?,
            };
            if let Event::Incoming(Packet::Publish(p)) = ev
                && let Ok(json) = serde_json::from_slice::<Value>(&p.payload)
            {
                state.apply(json.clone());
                if let Some(outcome) = session.observe(json) {
                    return Ok(outcome);
                }
            }
        }
    }

    async fn send_sequence_async<F: FnMut(usize)>(
        &self,
        commands: &[Command],
        mut before_step: F,
        settle: Option<Duration>,
    ) -> Result<SequenceRun, ClientError> {
        let (client, mut eventloop) = self.connect_ready().await?;
        // connect() already used sequence id "0" for the pushall.
        let mut ids = SequenceIds::new();
        let _ = ids.next_id();
        // Every report is merged here, so the settle phase can read the
        // printer's fields without a second connection.
        let mut state = ReportState::new();

        // A fault the sequence *causes* is only recognisable against a baseline
        // taken before it runs. Reading it afterwards would file the hook's own
        // crash under "pre-existing" and let the wait proceed as if nothing had
        // happened — so this snapshot has to precede the first line, not follow
        // the last.
        //
        // It also answers "is this printer still idle?" on THIS connection. The
        // caller's own idle check ran over a different one, minutes earlier for
        // an upload — long enough for a print to have been started from the
        // screen in between, and driving a plate swap into a running print is
        // the accident this whole path exists to avoid.
        let error_before = if settle.is_some() {
            let seq = ids.next_id();
            self.refresh_snapshot(&client, &mut eventloop, &seq, &mut state)
                .await?;
            // Stricter than the mid-sequence check: nothing has been published
            // yet, so "not known to be idle" is reason enough to stop.
            if let Some(why) = settle::blocks_start(gcode_state(&state)) {
                return Ok(SequenceRun {
                    outcomes: Vec::new(),
                    settled: Some(SettleOutcome::Interrupted {
                        reason: why.to_string(),
                    }),
                });
            }
            print_error(&state)
        } else {
            None
        };

        let mut outcomes = Vec::with_capacity(commands.len());
        // A job that starts *during* the sequence is the same accident found one
        // step later: the remaining lines would go to a printer that is now
        // running someone else's print.
        let mut took_over = None;
        for (i, cmd) in commands.iter().enumerate() {
            before_step(i);
            // A distinct id per step is what keeps the verdicts apart: reusing
            // one would let a repeat of the previous step's ACK verify the next
            // command without the printer having answered it.
            let seq = ids.next_id();
            let outcome = self
                .publish_and_verify(&client, &mut eventloop, &seq, cmd, &mut state)
                .await?;
            let confirmed = outcome == CommandOutcome::Verified;
            outcomes.push(outcome);
            // Stop rather than press on: the machine is mid-motion and the
            // remaining commands assume the earlier ones ran.
            if !confirmed {
                break;
            }
            // Reports merged while awaiting that ACK are the freshest view there
            // is, so the check costs nothing and catches a takeover one step in
            // rather than a whole sequence later.
            if settle.is_some() {
                took_over = settle::takeover(gcode_state(&state));
                if took_over.is_some() {
                    break;
                }
            }
        }

        // Only settle a run that actually finished: after a failed step the
        // machine is already stopped short, and the caller is going to abort.
        // `NotReached` rather than `None`, so "no wait was asked for" stays
        // distinguishable from "one was, and never got its chance".
        let all_ran = outcomes.len() == commands.len()
            && outcomes.iter().all(|o| *o == CommandOutcome::Verified);
        let settled = match settle {
            // A takeover names itself; falling through to `NotReached` would
            // report the vaguer of the two truths.
            Some(_) if took_over.is_some() => Some(SettleOutcome::Interrupted {
                reason: took_over.unwrap_or_default().to_string(),
            }),
            None => None,
            Some(_) if !all_ran => Some(SettleOutcome::NotReached),
            Some(budget) => {
                // The sentinel must dodge any stage the sequence sets for
                // itself: those lines are still queued and would look identical.
                let claimed = settle::claimed_stages(commands.iter().filter_map(|c| match c {
                    Command::GcodeLine(l) => Some(l.as_str()),
                    _ => None,
                }));
                Some(
                    self.settle_async(
                        &client,
                        &mut eventloop,
                        &mut ids,
                        &mut state,
                        budget,
                        &claimed,
                        error_before,
                    )
                    .await?,
                )
            }
        };
        Ok(SequenceRun { outcomes, settled })
    }

    /// Merge reports until a **full snapshot** has landed, so the state read
    /// straight after is the printer's, not whatever happened to be in flight.
    ///
    /// The connect-time `pushall` is not enough on its own: nothing waits for
    /// its response, so a fast sequence can reach the settle phase before the
    /// snapshot merges — and then a *stale* one arrives mid-wait carrying, say,
    /// the sentinel a previously-crashed run left behind. Asking again and
    /// blocking on the answer makes the reading authoritative: TCP order means
    /// every earlier report has already been merged by the time it arrives.
    async fn refresh_snapshot(
        &self,
        client: &AsyncClient,
        eventloop: &mut EventLoop,
        seq: &str,
        state: &mut ReportState,
    ) -> Result<(), ClientError> {
        client
            .publish(
                request_topic(&self.target.serial),
                QoS::AtLeastOnce,
                false,
                Command::PushAll.to_payload(seq).to_string(),
            )
            .await
            .map_err(|e| ClientError::Mqtt(e.to_string()))?;
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let ev = match tokio::time::timeout_at(deadline, poll(eventloop)).await {
                Err(_) => return Err(ClientError::Timeout(self.timeout)),
                Ok(ev) => ev?,
            };
            if let Event::Incoming(Packet::Publish(p)) = ev
                && let Ok(json) = serde_json::from_slice::<Value>(&p.payload)
            {
                let full = is_full_snapshot_message(&json);
                state.apply(json);
                if full {
                    return Ok(());
                }
            }
        }
    }

    /// Wait until the printer's G-code queue has drained (see [`crate::core::settle`]).
    ///
    /// Sends `M400` + an out-of-band `stg_cur` sentinel, watches the reports for
    /// that sentinel, then puts the displayed stage back — restoring even when
    /// the wait fails, so the machine is never left showing a value no decoder
    /// knows.
    #[allow(clippy::too_many_arguments)]
    async fn settle_async(
        &self,
        client: &AsyncClient,
        eventloop: &mut EventLoop,
        ids: &mut SequenceIds,
        state: &mut ReportState,
        budget: Duration,
        claimed: &[i64],
        error_before: Option<i64>,
    ) -> Result<SettleOutcome, ClientError> {
        // Establish where the printer *actually* is before choosing a sentinel
        // it must not already be showing. See `refresh_snapshot`.
        let seq = ids.next_id();
        self.refresh_snapshot(client, eventloop, &seq, state)
            .await?;
        let Some(stage_now) = stg_cur(state) else {
            // Without a reading there is no sentinel that can be trusted, and
            // guessing one risks ending the wait on a value that was already
            // there. Refusing is the safe direction.
            return Err(ClientError::Timeout(self.timeout));
        };
        // No value left whose appearance would prove anything. Refuse rather
        // than settle for a compromised one: a sentinel the sequence also
        // claims can land while the motion is still draining, which is the
        // whole failure this exists to prevent.
        let Some(settle) = Settle::after(stage_now, claimed, error_before) else {
            return Ok(SettleOutcome::NoSentinel {
                claimed: claimed.to_vec(),
            });
        };

        let mut sent_sentinel = false;
        let mut refused = None;
        for line in settle.gcode() {
            let seq = ids.next_id();
            let outcome = self
                .publish_and_verify(
                    client,
                    eventloop,
                    &seq,
                    &Command::GcodeLine(line.clone()),
                    state,
                )
                .await?;
            if outcome != CommandOutcome::Verified {
                // Either way the line may still execute and leave the sentinel
                // showing, so this still needs restoring — but the two are not
                // the same verdict. A rejection is the printer saying no; an
                // ACK that never arrived says nothing at all, and reporting it
                // as a refusal would name a cause that was never observed.
                sent_sentinel = true;
                refused = Some(match outcome {
                    CommandOutcome::Rejected { .. } => SettleOutcome::NotSent { gcode: line },
                    _ => SettleOutcome::NotConfirmed { gcode: line },
                });
                break;
            }
            sent_sentinel = true;
        }

        let result = match refused {
            Some(o) => o,
            None => {
                let deadline = tokio::time::Instant::now() + budget;
                loop {
                    // The sentinel may already be in a report that arrived while
                    // its own ACK was awaited, so classify before blocking.
                    match settle.observe(stg_cur(state), gcode_state(state), print_error(state)) {
                        SettleStep::Settled => break SettleOutcome::Settled,
                        SettleStep::Interrupted(why) => {
                            break SettleOutcome::Interrupted {
                                reason: why.to_string(),
                            };
                        }
                        SettleStep::Waiting => {}
                    }
                    let ev = match tokio::time::timeout_at(deadline, poll(eventloop)).await {
                        Err(_) => {
                            break SettleOutcome::TimedOut {
                                after_secs: budget.as_secs(),
                            };
                        }
                        Ok(ev) => ev?,
                    };
                    if let Event::Incoming(Packet::Publish(p)) = ev
                        && let Ok(json) = serde_json::from_slice::<Value>(&p.payload)
                    {
                        state.apply(json);
                    }
                }
            }
        };

        // Restore unless someone else now owns the printer: after a takeover the
        // displayed stage belongs to *their* job, and tidying ours away would
        // overwrite it. Read that from the printer's CURRENT state, not from the
        // verdict — `Interrupted` also covers a fault the sequence caused, and
        // that machine is still ours and still showing our sentinel. Otherwise
        // best-effort: the wait's verdict is what the caller acts on, so failing
        // to tidy up must not mask it.
        let took_over = settle::takeover(gcode_state(state)).is_some();
        if sent_sentinel && !took_over {
            let seq = ids.next_id();
            let _ = self
                .publish_and_verify(
                    client,
                    eventloop,
                    &seq,
                    &Command::GcodeLine(Settle::restore_gcode()),
                    state,
                )
                .await;
        }
        Ok(result)
    }

    /// Send `commands` in order over **one** connection, verifying each before
    /// the next is published.
    ///
    /// This is the multi-command counterpart to
    /// [`send_and_verify`](Self::send_and_verify), which reconnects (and
    /// re-`pushall`s) per call — looping that would breach the A1/P1
    /// single-client MQTT limit dozens of times for one macro.
    ///
    /// Returns one [`CommandOutcome`] per step **that got a verdict**: the run
    /// stops at the first step the printer doesn't confirm, so a short vector
    /// means it stopped at `outcomes.len()`. `before_step` is called with the
    /// 0-based index just before that step is published — the only way a caller
    /// can follow progress, and the step index to blame if this returns `Err`.
    ///
    /// There is deliberately **no** timeout over the whole run: a real sequence
    /// dwells (`G4 S3`) for minutes. The bounds are the connect and each step
    /// (both `timeout`).
    ///
    /// Only the first step sees the connect-time `pushall`, so only it can take
    /// a `print_error` baseline; a later step would read a *pre-existing* fault
    /// as one it caused. Harmless for the commands this exists for — a
    /// `gcode_line` has no observable effect, so its ACK is the whole verdict —
    /// but a sequence of effectful commands needs that baseline carried across
    /// steps first.
    /// With `settle` set, the run does not end at the last ACK: the queue is
    /// drained and observed first (see [`crate::core::settle`]), bounded by that
    /// duration. Pass `None` when the caller only needs the commands published —
    /// the ACKs say nothing about whether the machine has stopped moving.
    pub fn send_sequence<F: FnMut(usize)>(
        &self,
        commands: &[Command],
        before_step: F,
        settle: Option<Duration>,
    ) -> Result<SequenceRun, ClientError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        rt.block_on(self.send_sequence_async(commands, before_step, settle))
    }

    async fn send_fire_async(&self, cmd: &Command) -> Result<(), ClientError> {
        let (client, mut eventloop) = self.connect().await?;
        // connect() used sequence id "0" for the pushall; this command gets "1".
        client
            .publish(
                request_topic(&self.target.serial),
                QoS::AtLeastOnce,
                false,
                cmd.to_payload("1").to_string(),
            )
            .await
            .map_err(|e| ClientError::Mqtt(e.to_string()))?;
        // Pump the event loop briefly so the QoS-1 PUBLISH is actually written to
        // the wire before we drop the connection. A reboot then tears the
        // connection down (an error here is expected, not a failure).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            match tokio::time::timeout_at(deadline, poll(&mut eventloop)).await {
                Err(_) => break,     // pump window elapsed — publish has been flushed
                Ok(Ok(_)) => {}      // PUBACK or other events — keep pumping
                Ok(Err(_)) => break, // connection dropped (expected for reboot)
            }
        }
        Ok(())
    }

    /// Publish a command **fire-and-forget** — no ACK or effect is awaited. For
    /// commands whose effect can't be read back because they tear down the
    /// connection (e.g. [`Command::Reboot`]). Returns once the publish is flushed.
    pub fn send_fire(&self, cmd: &Command) -> Result<(), ClientError> {
        self.run_with_timeout(self.send_fire_async(cmd))
    }

    fn run_with_timeout<T, Fut>(&self, fut: Fut) -> Result<T, ClientError>
    where
        Fut: std::future::Future<Output = Result<T, ClientError>>,
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        rt.block_on(async {
            tokio::time::timeout(self.timeout, fut)
                .await
                .unwrap_or(Err(ClientError::Timeout(self.timeout)))
        })
    }
}

impl StatusSource for LanMqttClient {
    fn fetch_snapshot(&self) -> Result<ReportState, ClientError> {
        self.run_with_timeout(self.fetch_async())
    }
}

/// Poll the event loop, mapping errors to [`ClientError`].
async fn poll(eventloop: &mut EventLoop) -> Result<Event, ClientError> {
    eventloop
        .poll()
        .await
        .map_err(|e| ClientError::Mqtt(e.to_string()))
}

/// Build a rustls config that accepts the printer's self-signed certificate.
fn tls_config() -> Result<TlsConfiguration, ClientError> {
    let config = crate::tls::lan_client_config().map_err(|e| ClientError::Tls(e.to_string()))?;
    Ok(TlsConfiguration::Rustls(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_formatted_per_serial() {
        assert_eq!(report_topic("0309FA"), "device/0309FA/report");
        assert_eq!(request_topic("0309FA"), "device/0309FA/request");
    }

    #[test]
    fn tls_config_builds() {
        assert!(tls_config().is_ok());
    }

    #[test]
    fn client_ids_are_unique_per_connection() {
        let a = unique_client_id();
        let b = unique_client_id();
        assert!(a.starts_with("bambu-rs-"));
        assert_ne!(a, b); // distinct ids so concurrent connections don't collide
    }
}
