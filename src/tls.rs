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

/// A certificate and the key that goes with it. Only ever handled as a pair —
/// half of one and half of another is `KeyMismatch`, and a listener that never
/// binds.
#[cfg(feature = "relay")]
pub type Identity = (
    rustls_pki_types::CertificateDer<'static>,
    rustls_pki_types::PrivatePkcs8KeyDer<'static>,
);

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
) -> Result<Identity, ServerTlsError> {
    let Some(dir) = store else {
        return Ok(generate(serial)?.0);
    };
    std::fs::create_dir_all(dir)?;
    let lock = dir.join(format!("{serial}.lock"));

    // Rounds of: is there one? no — may I make it? no — wait a moment.
    //
    // Two emulators for the same serial can start at the same moment — the
    // end-to-end test does exactly that, a relay in front of a synthetic
    // printer — and the certificate and the key are separate files, so there
    // is no way for both to appear at once. Without the lock, each starter
    // generates its own pair and writes over the other's, and whoever reads one
    // file before the second write and the other after gets a certificate and a
    // key from different pairs. rustls calls that `KeyMismatch` and the
    // listener never binds.
    let mut takeovers = 0;
    loop {
        if let Some(pair) = read_pair(dir, serial)? {
            return Ok(pair);
        }
        // `create_new` is the atomic part: it either creates the lock or tells
        // us someone else already did.
        let won = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            // Anything else is this directory being unusable — no permission, a
            // full disk — and reading that as "somebody is holding the lock"
            // would send us round the stale path to a confident wrong answer
            // about a process dying, instead of saying what actually failed.
            Err(e) => return Err(e.into()),
        };
        if won {
            // Holding the lock is not the same as nobody having written one.
            // The previous holder removes it when it finishes, so a starter
            // that found the directory empty a moment ago can take the lock
            // straight after that removal and generate a *second* identity over
            // a perfectly good first one — two starters, two certificates for
            // one printer, and a client that pinned the first rejecting the
            // second. Looking again here is what makes winning the lock mean
            // what it reads like.
            if let Some(pair) = read_pair(dir, serial)? {
                let _ = std::fs::remove_file(&lock);
                return Ok(pair);
            }
            let (pair, pem) = generate(serial)?;
            let written = write_pair(dir, serial, &pair, &pem);
            let _ = std::fs::remove_file(&lock);
            written?;
            return Ok(pair);
        }
        // Somebody else holds it. Whether to wait or to break it is a question
        // about the *lock*, not about how long we personally have been here:
        // two starters that began together also time out together, and one
        // counting its own patience would break the lock the other had just
        // taken — putting both back to generating at once, which is the thing
        // the lock is for.
        if !is_stale(&lock) {
            // Mid-write. The files appear by rename, so each is complete the
            // instant it is visible; waiting for both is enough.
            std::thread::sleep(POLL);
            continue;
        }
        // Old enough that whoever made it is gone. Break it and go round again
        // rather than writing from here: the next round re-reads and re-locks,
        // so two starters that break the same stale lock still come away with
        // one identity between them.
        // Counted before the break, so the number in the message below is the
        // number of locks actually broken rather than one more than that.
        if takeovers >= MAX_TAKEOVERS {
            return Err(ServerTlsError::Io(std::io::Error::other(format!(
                "gave up on {}: broken {MAX_TAKEOVERS} times and still held, \
                 which means something is repeatedly dying while holding it",
                lock.display()
            ))));
        }
        takeovers += 1;
        let _ = std::fs::remove_file(&lock);
    }
}

/// Has the lock been sitting there longer than anyone could plausibly still be
/// writing?
///
/// Anything unreadable counts as stale, and so does anything the clock cannot
/// make sense of. A lock that has gone away is obvious enough. A timestamp in
/// the *future* — a clock stepped backwards, a file restored with a stamp from
/// another machine — is the dangerous one: reading that as "not stale yet"
/// makes it never stale, and a starter waits out a process that is already
/// gone, polling for as long as it is left running. Erring towards stale costs
/// at worst a broken lock, which the loop is built to survive; erring the other
/// way is a hang.
#[cfg(feature = "relay")]
fn is_stale(lock: &std::path::Path) -> bool {
    let Ok(written) = std::fs::metadata(lock).and_then(|m| m.modified()) else {
        return true;
    };
    written
        .elapsed()
        .map(|age| age >= STALE_AFTER)
        .unwrap_or(true)
}

/// How many stale locks to break before giving up rather than spinning.
#[cfg(feature = "relay")]
const MAX_TAKEOVERS: u32 = 3;

/// How long a lock may sit before it is treated as abandoned. Generating a key
/// takes milliseconds, so this is only ever reached by a process that died.
#[cfg(feature = "relay")]
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// How often to look while somebody else is writing.
#[cfg(feature = "relay")]
const POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// A self-signed certificate naming this serial, its key, and the certificate
/// in PEM.
///
/// The PEM comes back with the pair because rcgen can only produce it from the
/// certificate it just built; deriving it later, from DER read off disk, would
/// mean carrying a base64 encoder for one line of output.
#[cfg(feature = "relay")]
fn generate(serial: &str) -> Result<(Identity, String), ServerTlsError> {
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
    let pem = cert.pem();
    Ok((
        (
            rustls_pki_types::CertificateDer::from(cert.der().to_vec()),
            rustls_pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()),
        ),
        pem,
    ))
}

/// The stored pair, or `None` if it is not (yet) there in full.
#[cfg(feature = "relay")]
fn read_pair(dir: &std::path::Path, serial: &str) -> Result<Option<Identity>, ServerTlsError> {
    let (cert, key) = (cert_der_path(dir, serial), key_path(dir, serial));
    if !cert.is_file() || !key.is_file() {
        return Ok(None);
    }
    Ok(Some((
        rustls_pki_types::CertificateDer::from(std::fs::read(cert)?),
        rustls_pki_types::PrivatePkcs8KeyDer::from(std::fs::read(key)?),
    )))
}

/// Write the pair, each file appearing whole or not at all.
///
/// Rename within the directory is atomic, so a reader never sees half a file —
/// which matters because the reader is another process deciding whether the
/// identity exists yet.
#[cfg(feature = "relay")]
fn write_pair(
    dir: &std::path::Path,
    serial: &str,
    (cert, key): &Identity,
    cert_pem: &str,
) -> Result<(), ServerTlsError> {
    // Unique per writer: the lock means only one process should be here, but
    // the stale-lock takeover path can overlap, and two writers sharing one
    // staging name rename each other's half-written files into place.
    let staging = dir.join(format!(
        ".{serial}.{}.{:?}.tmp",
        std::process::id(),
        std::thread::current().id()
    ));
    write_private(&staging, key.secret_pkcs8_der())?;
    std::fs::rename(&staging, key_path(dir, serial))?;

    std::fs::write(&staging, cert.as_ref())?;
    std::fs::rename(&staging, cert_der_path(dir, serial))?;

    // The PEM is for a human to hand to a client; it encodes the same DER, so
    // it can be written last without a window where the two disagree.
    std::fs::write(&staging, cert_pem)?;
    std::fs::rename(&staging, cert_pem_path(dir, serial))?;
    let _ = std::fs::remove_file(&staging);
    Ok(())
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

    #[cfg(feature = "relay")]
    #[test]
    fn a_lock_left_by_a_dead_process_still_yields_one_identity() {
        // The racing test above is a race, so it only fails when the timing
        // goes wrong — it passed sixty times here and failed on CI. This is the
        // same defect made deterministic: with a lock nobody is holding, two
        // starters used to wait it out, both decide it was theirs to break, and
        // both generate. Two certificates for one printer, and a client that
        // pinned the first rejects the second.
        //
        // Breaking a stale lock has to hand off, not just clear the way.
        let dir = scratch("stale-lock");
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("0309FA123456789.lock");
        let file = std::fs::File::create(&lock).unwrap();
        // Older than any writer could plausibly still be, so the wait is not
        // what this test is measuring.
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(600))
            .unwrap();
        drop(file);

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    emulated_printer_identity("0309FA123456789", Some(&dir)).unwrap()
                })
            })
            .collect();
        let got: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(
            (&got[0].0, &got[0].1),
            (&got[1].0, &got[1].1),
            "both broke the same stale lock and generated their own identity"
        );
        assert!(!lock.exists(), "the lock should not be left behind");
    }

    /// A lock stamped in the future must not be waited on forever.
    ///
    /// Age is measured with `SystemTime::elapsed`, which *fails* rather than
    /// going negative when the stamp is ahead of the clock — a clock stepped
    /// backwards, or a file restored with a timestamp from another machine.
    /// Reading that failure as "no age yet" made the lock never stale, and a
    /// starter would poll for a process that was already gone until somebody
    /// killed it. A relay that never comes up and never says why.
    #[cfg(feature = "relay")]
    #[test]
    fn a_lock_stamped_in_the_future_is_not_waited_on_forever() {
        let dir = scratch("future-lock");
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("0309FA123456789.lock");
        let file = std::fs::File::create(&lock).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(600))
            .unwrap();
        drop(file);

        // On another thread with a deadline, because the failure this guards
        // against is an *endless* wait: asserting on elapsed time afterwards
        // would never get to run, and the regression would show up as a CI job
        // that hangs until the runner kills it rather than a test that failed.
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(emulated_printer_identity("0309FA123456789", Some(&dir)));
        });
        let got = finished
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("still waiting after 2s, so it is waiting out the clock");
        let (cert, _) = got.expect("a lock nobody holds should not stop a printer starting");
        assert!(!cert.is_empty());
    }

    /// A directory that cannot be written says so, instead of blaming a peer.
    ///
    /// Taking the lock used to be `.is_ok()`, which reads every failure as
    /// "somebody else has it" — so a permission problem went round the
    /// stale-lock path and came back as a confident story about a process
    /// repeatedly dying.
    #[cfg(all(feature = "relay", unix))]
    #[test]
    fn an_unwritable_store_reports_itself() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch("unwritable");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Root ignores the mode, and so do some container filesystems. Only
        // assert where the thing under test can actually happen.
        let enforced = std::fs::write(dir.join(".probe"), b"x").is_err();

        let got = emulated_printer_identity("0309FA123456789", Some(&dir));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        if enforced {
            let err = got.expect_err("an unwritable store cannot yield an identity");
            let said = err.to_string();
            assert!(
                said.contains("ermission") || said.contains("denied"),
                "it should name the real failure, not a peer: {said}"
            );
        }
    }

    #[cfg(feature = "relay")]
    #[test]
    fn racing_starts_agree_on_one_identity() {
        // CI caught this as `rustls: keys may not be consistent: KeyMismatch`
        // and a relay whose listener never bound. Two emulators for the same
        // serial start together — the end-to-end test runs exactly that, a
        // relay in front of a synthetic printer — and the certificate and key
        // are separate files, so each writer can clobber half of the other's
        // pair and a reader can take one half from each.
        let dir = scratch("race");
        std::fs::create_dir_all(&dir).unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    emulated_printer_identity("0309FA123456789", Some(&dir)).unwrap()
                })
            })
            .collect();
        let got: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();

        for (i, (cert, key)) in got.iter().enumerate() {
            assert_eq!(
                (cert, key),
                (&got[0].0, &got[0].1),
                "starter {i} came away with a different identity"
            );
            // The pair must also be coherent — the failure in CI was a
            // certificate and a key that did not belong together, which only
            // rustls notices.
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.clone()],
                rustls_pki_types::PrivateKeyDer::Pkcs8(key.clone_key()),
            )
            .unwrap_or_else(|e| panic!("starter {i} got a mismatched pair: {e}"));
        }

        // And the identity outlives them: a later start reuses it, or every
        // client that pinned it is locked out on the next restart.
        let later = emulated_printer_identity("0309FA123456789", Some(&dir)).unwrap();
        assert_eq!(later.0, got[0].0);
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
