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
use crate::core::report::{ReportState, is_full_snapshot_message};
use crate::core::session::VerifySession;
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
    fn client_ids_are_unique_per_connection() {
        let a = unique_client_id();
        let b = unique_client_id();
        assert!(a.starts_with("bambu-rs-"));
        assert_ne!(a, b); // distinct ids so concurrent connections don't collide
    }
}
