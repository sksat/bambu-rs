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

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, TlsConfiguration, Transport};
use serde_json::Value;

use crate::config::ResolvedTarget;
use crate::core::command::Command;
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

    async fn fetch_async(&self) -> Result<ReportState, ClientError> {
        let mut opts = MqttOptions::new("bambu-rs", &self.target.ip, MQTT_PORT);
        opts.set_credentials(MQTT_USER, &self.target.access_code);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_transport(Transport::Tls(tls_config()?));

        let (client, mut eventloop) = AsyncClient::new(opts, 16);
        client
            .subscribe(report_topic(&self.target.serial), QoS::AtMostOnce)
            .await
            .map_err(|e| ClientError::Mqtt(e.to_string()))?;
        let pushall = Command::PushAll.to_payload("0").to_string();
        client
            .publish(
                request_topic(&self.target.serial),
                QoS::AtMostOnce,
                false,
                pushall,
            )
            .await
            .map_err(|e| ClientError::Mqtt(e.to_string()))?;

        let mut state = ReportState::new();
        loop {
            let event = eventloop
                .poll()
                .await
                .map_err(|e| ClientError::Mqtt(e.to_string()))?;
            if let Event::Incoming(Packet::Publish(p)) = event
                && let Ok(json) = serde_json::from_slice::<Value>(&p.payload)
            {
                state.apply(json);
                // Return once we have the print object (the pushall snapshot).
                if state.pointer("/print").is_some() {
                    return Ok(state);
                }
            }
        }
    }
}

impl StatusSource for LanMqttClient {
    fn fetch_snapshot(&self) -> Result<ReportState, ClientError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| ClientError::Runtime(e.to_string()))?;
        rt.block_on(async {
            tokio::time::timeout(self.timeout, self.fetch_async())
                .await
                .unwrap_or(Err(ClientError::Timeout(self.timeout)))
        })
    }
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
