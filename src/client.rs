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

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{
    AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS, TlsConfiguration, Transport,
};
use serde_json::Value;

use crate::config::ResolvedTarget;
use crate::core::command::{Command, SequenceIds};
use crate::core::report::ReportState;
use crate::core::status::PrinterStatus;
use crate::core::verify::{self, EffectStatus};
use crate::core::version::DeviceVersion;

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

/// Which stage of verification failed to confirm a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyStage {
    /// No usable ACK arrived (the printer never echoed our `sequence_id`).
    Ack,
    /// The ACK was `success`, but the command's *effect* was never observed in
    /// the report before the timeout (e.g. a print that never started).
    Effect,
}

/// The result of verifying a control command.
///
/// The printer echoes a command's `sequence_id` back with `result`/`reason`
/// (observed on the A1 mini); the ACK is necessary but **not sufficient**. For
/// commands with an observable effect (a print start, pause/resume/stop) we also
/// read the report back and confirm the effect — and watch for a new
/// `print_error` — because an ACK of `success` can still be followed by nothing
/// happening (observed: a failing SD card ACKed `project_file` then never
/// printed). See [`crate::core::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// ACKed `success` **and**, for effectful commands, the effect was observed.
    Verified,
    /// The printer rejected the command (ACK `result != success`) or a new
    /// device error appeared right after it. `reason` is human-readable.
    Rejected { reason: String },
    /// Sent but not confirmed — never assume success. `stage` says whether we
    /// never saw an ACK ([`VerifyStage::Ack`]) or saw the ACK but never the
    /// effect ([`VerifyStage::Effect`]).
    Unverified { stage: VerifyStage },
}

/// The report topic for a serial: `device/{serial}/report`.
pub fn report_topic(serial: &str) -> String {
    format!("device/{serial}/report")
}

/// The request topic for a serial: `device/{serial}/request`.
pub fn request_topic(serial: &str) -> String {
    format!("device/{serial}/request")
}

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
        let mut opts = MqttOptions::new("bambu-rs", &self.target.ip, MQTT_PORT);
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
                state.apply(json);
                // Wait for the actual pushall response (command == "push_status"),
                // not an unsolicited delta that merely carries a `print` object: a
                // delta would be a partial snapshot missing most fields.
                if is_full_snapshot(&state) {
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

        // The ACK comes back under the command's own category (print/system/…).
        let cat = cmd.category();
        let seq_ptr = format!("/{cat}/sequence_id");
        let result_ptr = format!("/{cat}/result");
        let reason_ptr = format!("/{cat}/reason");

        // Per-phase budget starts after connect, so verification gets the full
        // configured timeout regardless of how long connecting took. The outer
        // net in send_and_verify guards a connect/network hang.
        let deadline = tokio::time::Instant::now() + self.timeout;

        let mut state = ReportState::new();
        let mut acked = false;
        // print_error as it stood before the command took effect, so we react
        // only to a NEW fault (the pushall snapshot supplies it).
        let mut baseline_error: Option<i64> = None;

        loop {
            let ev = match tokio::time::timeout_at(deadline, poll(&mut eventloop)).await {
                Err(_) => {
                    // Verify timeout: distinguish "never ACKed" from "ACKed but
                    // the effect never showed".
                    let stage = if acked {
                        VerifyStage::Effect
                    } else {
                        VerifyStage::Ack
                    };
                    return Ok(CommandOutcome::Unverified { stage });
                }
                Ok(ev) => ev?,
            };

            let Event::Incoming(Packet::Publish(p)) = ev else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<Value>(&p.payload) else {
                continue;
            };
            state.apply(json);

            // Capture the baseline print_error the first time we see the full
            // pushall snapshot.
            if baseline_error.is_none() && is_full_snapshot(&state) {
                baseline_error = Some(
                    PrinterStatus::from_state(state.get())
                        .print_error
                        .unwrap_or(0),
                );
            }

            // Phase 1 — the ACK echoes our sequence_id and carries result/reason.
            if !acked {
                let echoed = state.pointer(&seq_ptr).and_then(|v| v.as_str()) == Some(seq);
                if let (true, Some(result)) =
                    (echoed, state.pointer(&result_ptr).and_then(|v| v.as_str()))
                {
                    if !result.eq_ignore_ascii_case("success") {
                        let reason = state
                            .pointer(&reason_ptr)
                            .and_then(|v| v.as_str())
                            .unwrap_or(result)
                            .to_string();
                        return Ok(CommandOutcome::Rejected { reason });
                    }
                    acked = true;
                    // For commands with no readable effect, the ACK is final.
                    if !verify::has_observable_effect(cmd) {
                        return Ok(CommandOutcome::Verified);
                    }
                }
            }

            // Phase 2 — confirm the effect actually happened (and no new fault).
            if acked {
                let status = PrinterStatus::from_state(state.get());
                match verify::evaluate(cmd, &status, baseline_error) {
                    EffectStatus::Observed => return Ok(CommandOutcome::Verified),
                    EffectStatus::NewError(code) => {
                        return Ok(CommandOutcome::Rejected {
                            reason: format!(
                                "device reported error 0x{code:08X} after the command; \
                                 the effect was not observed (state unchanged)"
                            ),
                        });
                    }
                    EffectStatus::Pending => {}
                }
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

/// Whether the merged state holds the full `pushall` response.
///
/// Observed on the A1 mini (`tools/capture_report_delta.py`): the full snapshot
/// carries `print.msg == 0` with all ~64 fields, while periodic **deltas** carry
/// `print.msg == 1` with only the changed fields — and **both** set
/// `command == "push_status"`. So the command alone is not a reliable
/// full-vs-delta signal; `msg == 0` is. We treat it as full when it's a
/// `push_status` and `msg` is either absent (older firmware that may not send it)
/// or zero, so a delta (`msg == 1`) arriving first is not mistaken for the snapshot.
fn is_full_snapshot(state: &ReportState) -> bool {
    let print = state.pointer("/print");
    let is_push_status =
        print.and_then(|p| p.get("command")).and_then(|v| v.as_str()) == Some("push_status");
    let msg = print.and_then(|p| p.get("msg")).and_then(|v| v.as_i64());
    is_push_status && msg.is_none_or(|m| m == 0)
}

/// Build a rustls config that accepts the printer's self-signed certificate.
fn tls_config() -> Result<TlsConfiguration, ClientError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| ClientError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptSelfSigned(provider)))
        .with_no_client_auth();
    Ok(TlsConfiguration::Rustls(Arc::new(config)))
}

/// A deliberately-insecure verifier for the printer's LAN TLS — the rustls
/// equivalent of OpenSSL's `CERT_NONE`.
///
/// Why: Bambu printers present a **self-signed X.509 *version 1*** certificate
/// (CN = the serial, issuer "BBL CA"). rustls/webpki reject v1 certificates with
/// `UnsupportedCertVersion`, and they have no CA chain anyway. Since the printer
/// is reached by IP on the LAN with an out-of-band access code, we accept any
/// certificate and skip handshake-signature validation (the encrypted channel
/// still comes from the ephemeral key exchange). `supported_verify_schemes` is
/// kept honest so the server picks a scheme we can negotiate.
///
/// This trades server authentication for connectivity; it is acceptable only for
/// the LAN-direct, self-signed printer case.
#[derive(Debug)]
struct AcceptSelfSigned(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptSelfSigned {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // Skipped on purpose: webpki cannot parse the v1 cert to extract the key.
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
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
    fn full_snapshot_is_msg_zero_not_just_push_status() {
        use serde_json::json;
        let with = |v| {
            let mut rs = ReportState::new();
            rs.apply(v);
            is_full_snapshot(&rs)
        };
        // Full pushall response: push_status + msg 0.
        assert!(with(json!({ "print": { "command": "push_status", "msg": 0 } })));
        // A delta also says push_status but msg == 1 -> NOT the full snapshot.
        assert!(!with(json!({ "print": { "command": "push_status", "msg": 1 } })));
        // Older firmware without msg: fall back to the command check.
        assert!(with(json!({ "print": { "command": "push_status" } })));
        // Not a push_status at all.
        assert!(!with(json!({ "print": { "command": "gcode_line" } })));
    }
}
