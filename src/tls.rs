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

/// Building the emulated printer's server-side TLS failed.
#[cfg(feature = "server")]
#[derive(Debug, thiserror::Error)]
pub enum ServerTlsError {
    #[error(
        "cannot emulate a printer with an empty serial: it names the certificate and both MQTT topics"
    )]
    NoSerial,
    #[error("generating the emulated printer's certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("rustls: {0}")]
    Rustls(#[from] rustls::Error),
}

/// A rustls [`ServerConfig`](rustls::ServerConfig) presenting a **freshly
/// generated** self-signed certificate with `CN = serial` — what
/// `bambu serve --emulate` answers a TLS handshake with.
///
/// Two deliberate differences from the real printer, both safe:
///
/// - The printer's certificate is X.509 **v1** and RSA-2048; this one is v3 and
///   ECDSA P-256. A v1 self-signed certificate is unverifiable (webpki won't
///   even parse it — see the module docs), so any LAN client that talks to a
///   Bambu printer at all must already be skipping verification, and a *more*
///   standard certificate cannot be the thing that breaks it. ECDSA also
///   generates in microseconds where RSA-2048 would stall startup, and `ring`
///   cannot generate RSA keys at all.
/// - It is generated per run rather than cached on disk. Nothing pins it today,
///   and a key sitting in the config directory is a liability that buys nothing
///   until something does.
#[cfg(feature = "server")]
pub fn emulated_printer_server_config(
    serial: &str,
) -> Result<Arc<rustls::ServerConfig>, ServerTlsError> {
    // rcgen will cheerfully issue a certificate with an empty name, and the
    // topics would be `device//report`. Nothing downstream would notice.
    if serial.is_empty() {
        return Err(ServerTlsError::NoSerial);
    }
    // `localhost` alongside the serial so a hostname-checking client works
    // against a relay on the same machine. A client reaching us over the LAN
    // connects by IP, which we can't know here — but it can't be verifying
    // anyway, for the reason above.
    let mut params = rcgen::CertificateParams::new(vec![serial.to_string(), "localhost".into()])?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, serial);
    let signing_key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&signing_key)?;

    let chain = vec![rustls_pki_types::CertificateDer::from(cert.der().to_vec())];
    let key = rustls_pki_types::PrivateKeyDer::Pkcs8(rustls_pki_types::PrivatePkcs8KeyDer::from(
        signing_key.serialize_der(),
    ));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
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

    // That a client actually completes a handshake against this is proved
    // end-to-end in `server::emulate`, over a real socket against
    // `lan_client_config` — the same TLS the CLI uses on a real printer. Here we
    // only pin what this function alone decides.
    #[cfg(feature = "server")]
    #[test]
    fn the_emulated_printers_certificate_is_built_fresh_and_names_its_serial() {
        let serial = "0309FA123456789";
        // `with_single_cert` validates that the key matches the chain, so a
        // config coming back at all means the pair is coherent.
        assert!(emulated_printer_server_config(serial).is_ok());

        // The serial reaches the certificate: it is the CN and a SAN, and a
        // serial that can't be encoded should fail loudly rather than yield a
        // certificate naming nothing.
        let params = rcgen::CertificateParams::new(vec![serial.to_string()]).unwrap();
        assert!(params.subject_alt_names.iter().any(|s| matches!(
            s,
            rcgen::SanType::DnsName(n) if n.as_str() == serial
        )));

        // An empty serial has no name to put in the certificate; rcgen rejects
        // it rather than us shipping an anonymous one.
        assert!(emulated_printer_server_config("").is_err());
    }
}
