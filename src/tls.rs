//! LAN TLS for the printer's self-signed X.509 **v1** certificate — shared by the MQTT
//! client, FTPS, and the camera so the whole crate uses one TLS stack (rustls; no OpenSSL).
//!
//! Bambu printers present a self-signed **version 1** certificate (CN = the serial, issuer
//! "BBL CA"). rustls/webpki reject v1 certificates with `UnsupportedCertVersion`, and they
//! have no CA chain anyway. Since the printer is reached by IP on the LAN with an
//! out-of-band access code, we accept any certificate and skip handshake-signature
//! validation — the rustls equivalent of OpenSSL's `CERT_NONE`. This trades server
//! authentication for connectivity; acceptable only for the LAN-direct, self-signed case.
//!
//! The verifier must override `verify_server_cert` **and** both signature methods: a
//! partial verifier that inspects the certificate still trips the v1 rejection (webpki
//! can't parse it to extract the key).

use std::sync::Arc;

/// A rustls [`ClientConfig`](rustls::ClientConfig) that accepts the printer's self-signed
/// v1 certificate. Built with the ring provider, matching the rest of the TLS stack.
pub fn lan_client_config() -> Result<Arc<rustls::ClientConfig>, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptSelfSigned(provider)))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

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
    fn config_builds() {
        assert!(lan_client_config().is_ok());
    }
}
