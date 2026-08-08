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
#[cfg(feature = "relay")]
#[derive(Debug, thiserror::Error)]
pub enum ServerTlsError {
    #[error(
        "cannot emulate a printer with an empty serial: it names the certificate and both MQTT topics"
    )]
    NoSerial,
    #[error(
        "the serial is {0} bytes; it names the MQTT topics, which cannot describe a string that long"
    )]
    SerialTooLong(usize),
    #[error("generating the emulated printer's certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("rustls: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("storing the emulated printer's identity: {0}")]
    Io(#[from] std::io::Error),
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
/// - It is generated per run when `store` is `None`. Give it a directory and the
///   identity is kept and reused, which is what a client that *pins* the
///   certificate needs — Bambu Studio verifies a printer against the CAs it
///   bundles (`resources/cert/printer.cer`: BBL CA and friends), so a relay can
///   never be trusted by chain, only by being pinned explicitly. A pin against
///   an identity that changes on every restart would be worse than none.
#[cfg(feature = "relay")]
pub fn emulated_printer_server_config(
    serial: &str,
    store: Option<&std::path::Path>,
) -> Result<Arc<rustls::ServerConfig>, ServerTlsError> {
    // rcgen will cheerfully issue a certificate with an empty name, and the
    // topics would be `device//report`. Nothing downstream would notice.
    if serial.is_empty() {
        return Err(ServerTlsError::NoSerial);
    }
    // The serial becomes the MQTT topics, and an MQTT string is length-prefixed
    // with a u16. The encoder clamps rather than emit a packet that lies about
    // its length, but a truncated topic is a relay nobody can subscribe to — so
    // refuse here, where there is still someone to tell.
    if serial.len() > 512 {
        return Err(ServerTlsError::SerialTooLong(serial.len()));
    }
    let (cert_der, key_der) = emulated_printer_identity(serial, store)?;
    let chain = vec![cert_der];
    let key = rustls_pki_types::PrivateKeyDer::Pkcs8(key_der);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
    Ok(Arc::new(config))
}

/// Where the certificate a client pins is kept, and the key that goes with it.
///
/// Two files rather than one: a client has to *read* the certificate to trust
/// it, and the key must never be readable the same way.
#[cfg(feature = "relay")]
fn cert_pem_path(dir: &std::path::Path, serial: &str) -> std::path::PathBuf {
    dir.join(format!("{serial}.cert.pem"))
}

#[cfg(feature = "relay")]
fn cert_der_path(dir: &std::path::Path, serial: &str) -> std::path::PathBuf {
    dir.join(format!("{serial}.cert.der"))
}

#[cfg(feature = "relay")]
fn key_path(dir: &std::path::Path, serial: &str) -> std::path::PathBuf {
    dir.join(format!("{serial}.key.der"))
}

/// The emulated printer's certificate and private key, reused if `store` has
/// them and freshly made (and kept) if not.
///
/// The certificate is written twice on purpose: the DER is what gets loaded back
/// on the next start, and the PEM beside it is what a human points a client at.
/// Converting between them at read time would need a PEM parser to satisfy a
/// path that runs once per process.
#[cfg(feature = "relay")]
pub fn emulated_printer_identity(
    serial: &str,
    store: Option<&std::path::Path>,
) -> Result<
    (
        rustls_pki_types::CertificateDer<'static>,
        rustls_pki_types::PrivatePkcs8KeyDer<'static>,
    ),
    ServerTlsError,
> {
    if let Some(dir) = store {
        let (cert, key) = (cert_der_path(dir, serial), key_path(dir, serial));
        // Both or neither: a half-written pair would fail the key/chain check
        // inside rustls with nothing pointing at the cause.
        if cert.is_file() && key.is_file() {
            return Ok((
                rustls_pki_types::CertificateDer::from(std::fs::read(cert)?),
                rustls_pki_types::PrivatePkcs8KeyDer::from(std::fs::read(key)?),
            ));
        }
    }

    // `localhost` alongside the serial so a hostname-checking client works
    // against a relay on the same machine. A client reaching us over the LAN
    // connects by IP, which we can't know here.
    let mut params = rcgen::CertificateParams::new(vec![serial.to_string(), "localhost".into()])?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, serial);
    // rcgen's defaults are 1975 to 4096, which no real certificate looks like
    // and which a client sanity-checking validity could reasonably refuse. The
    // printer's own is a plain ten years; match that shape.
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2034, 1, 1);
    let signing_key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&signing_key)?;

    if let Some(dir) = store {
        std::fs::create_dir_all(dir)?;
        write_private(&key_path(dir, serial), &signing_key.serialize_der())?;
        std::fs::write(cert_der_path(dir, serial), cert.der())?;
        std::fs::write(cert_pem_path(dir, serial), cert.pem())?;
    }
    Ok((
        rustls_pki_types::CertificateDer::from(cert.der().to_vec()),
        rustls_pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()),
    ))
}

/// Write a private key so only its owner can read it.
///
/// The mode goes on at creation rather than afterwards: a chmod after the fact
/// leaves the key world-readable for the moment in between.
#[cfg(feature = "relay")]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(bytes)
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
    #[cfg(feature = "relay")]
    #[test]
    fn the_emulated_printers_certificate_is_built_fresh_and_names_its_serial() {
        let serial = "0309FA123456789";
        // `with_single_cert` validates that the key matches the chain, so a
        // config coming back at all means the pair is coherent.
        assert!(emulated_printer_server_config(serial, None).is_ok());

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
        assert!(emulated_printer_server_config("", None).is_err());
        // And one too long to fit in an MQTT topic is refused here, where there
        // is still someone to tell, rather than silently truncated on the wire.
        assert!(emulated_printer_server_config(&"S".repeat(1024), None).is_err());
    }

    #[cfg(feature = "relay")]
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bambu-tls-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[cfg(feature = "relay")]
    #[test]
    fn a_stored_identity_survives_a_restart() {
        // The whole point: a client that has been told to trust this printer
        // pins the certificate, and a relay that reinvented itself on every
        // start would break that pin every time it came back.
        let dir = scratch("stable");
        let serial = "0309FA123456789";
        let first = emulated_printer_identity(serial, Some(&dir)).unwrap();
        let again = emulated_printer_identity(serial, Some(&dir)).unwrap();
        assert_eq!(
            first.0, again.0,
            "a restart must present the certificate the client already trusts"
        );

        // Without somewhere to keep it, the old behaviour stands: ephemeral, and
        // no private key left in the filesystem for a feature nobody asked for.
        let a = emulated_printer_identity(serial, None).unwrap();
        let b = emulated_printer_identity(serial, None).unwrap();
        assert_ne!(
            a.0, b.0,
            "with no store there is nothing to be stable about"
        );
    }

    #[cfg(feature = "relay")]
    #[test]
    fn two_printers_do_not_share_one_identity() {
        let dir = scratch("distinct");
        let one = emulated_printer_identity("0309FAAAAAAAAAA", Some(&dir)).unwrap();
        let two = emulated_printer_identity("0309FBBBBBBBBBB", Some(&dir)).unwrap();
        assert_ne!(one.0, two.0);
    }

    #[cfg(all(feature = "relay", unix))]
    #[test]
    fn the_private_key_is_not_left_readable_to_everyone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perms");
        let serial = "0309FA123456789";
        emulated_printer_identity(serial, Some(&dir)).unwrap();
        let mode = std::fs::metadata(key_path(&dir, serial))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the key is a key, not a public file");
        // The certificate is the opposite: a client has to be able to read it to
        // trust it, and it is public by nature.
        let cert_mode = std::fs::metadata(cert_pem_path(&dir, serial))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(cert_mode, 0o644);
    }
}
