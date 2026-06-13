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

const MQTT_PORT: u16 = 8883;
const MQTT_USER: &str = "bblp";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors from the I/O client. Messages never include the access code.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error("MQTT error: {0}")]
    Mqtt(String),
    #[error("timed out after {0:?} waiting for a status snapshot")]
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

/// The result of verifying a control command against the printer's ACK.
///
/// The printer echoes a command's `sequence_id` back with `result`/`reason`
/// (observed on the A1 mini), so we can distinguish a confirmed command from one
/// that was merely published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The printer ACKed with `result == "success"`.
    Verified,
    /// The printer ACKed with a non-success result.
    Rejected { reason: String },
    /// Published, but no ACK arrived before the timeout — do NOT assume success.
    SentUnverified,
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

    async fn watch_async<F: FnMut(&ReportState) -> WatchStep>(
        &self,
        mut on_update: F,
    ) -> Result<ReportState, ClientError> {
        let (_client, mut eventloop) = self.connect().await?;
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

    /// Watch the printer, invoking `on_update` on every merged report until it
    /// returns [`WatchStep::Stop`], or the timeout elapses.
    pub fn watch<F: FnMut(&ReportState) -> WatchStep>(
        &self,
        on_update: F,
    ) -> Result<ReportState, ClientError> {
        self.run_with_timeout(self.watch_async(on_update))
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

        let mut state = ReportState::new();
        loop {
            if let Event::Incoming(Packet::Publish(p)) = poll(&mut eventloop).await?
                && let Ok(json) = serde_json::from_slice::<Value>(&p.payload)
            {
                state.apply(json);
                // The ACK echoes our sequence_id and carries result/reason.
                let echoed = state.pointer(&seq_ptr).and_then(|v| v.as_str()) == Some(seq);
                if let (true, Some(result)) =
                    (echoed, state.pointer(&result_ptr).and_then(|v| v.as_str()))
                {
                    return Ok(if result.eq_ignore_ascii_case("success") {
                        CommandOutcome::Verified
                    } else {
                        let reason = state
                            .pointer(&reason_ptr)
                            .and_then(|v| v.as_str())
                            .unwrap_or(result)
                            .to_string();
                        CommandOutcome::Rejected { reason }
                    });
                }
            }
        }
    }

    /// Send a control command and verify it against the printer's ACK (matched
    /// by echoed `sequence_id` + `result`). A timeout means **`SentUnverified`**
    /// — published but not confirmed — never assume success.
    pub fn send_and_verify(&self, cmd: &Command) -> Result<CommandOutcome, ClientError> {
        match self.run_with_timeout(self.send_and_verify_async(cmd)) {
            Ok(outcome) => Ok(outcome),
            Err(ClientError::Timeout(_)) => Ok(CommandOutcome::SentUnverified),
            Err(e) => Err(e),
        }
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

/// Whether the merged state holds the full `pushall` response, which the printer
/// marks with `command == "push_status"` (as opposed to a partial delta).
fn is_full_snapshot(state: &ReportState) -> bool {
    state.pointer("/print/command").and_then(|v| v.as_str()) == Some("push_status")
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
}
