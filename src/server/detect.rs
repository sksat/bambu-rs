//! The device-detect listener — the port Bambu Studio knocks on first.
//!
//! Studio's "add by IP" dialog probes TCP 3000 before it will open MQTT, and
//! abandons the whole attempt if nothing answers (see [`crate::core::detect`]).
//! Without this the relay is invisible to that dialog no matter how well it
//! serves 8883 — and invisibly so, because a refused SYN leaves no trace on the
//! server side.
//!
//! The reply is **relayed from the real printer** rather than composed here.
//! The probe asks what model, what firmware, what name — and the honest answers
//! are the printer's own. Making them up would mean inventing a firmware version
//! for a machine standing right there.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::core::detect::{self, MAX_FRAME};

/// A probe should be answered in well under this; Studio does not wait long.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a peer may dawdle over one frame.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// How many probes may be in flight at once.
///
/// A probe is one small round trip, so real traffic never approaches this. The
/// cap is here because the proxied variant occupies a blocking thread while it
/// waits on the printer: without a bound, a peer opening connections in a loop
/// would starve the blocking pool that the rest of `serve` also uses.
const MAX_IN_FLIGHT: usize = 32;

/// Where a detect reply comes from.
pub trait DetectSource: Send + Sync + 'static {
    /// Answer `request` — the decoded enquiry — with the reply frame's JSON.
    fn detect(&self, request: &serde_json::Value) -> anyhow::Result<serde_json::Value>;
}

/// Ask the real printer and pass its answer through.
pub struct ProxyDetect {
    addr: String,
}

impl ProxyDetect {
    pub fn new(printer_ip: &str, port: u16) -> Arc<Self> {
        Arc::new(Self {
            addr: format!("{printer_ip}:{port}"),
        })
    }
}

impl DetectSource for ProxyDetect {
    fn detect(&self, request: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        // Blocking, and deliberately so: this runs on a blocking task, and the
        // exchange is one small round trip that the printer either answers
        // promptly or not at all.
        use std::io::{Read, Write};
        let addr: std::net::SocketAddr = self.addr.parse().or_else(|_| -> anyhow::Result<_> {
            use std::net::ToSocketAddrs;
            self.addr
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| anyhow::anyhow!("{} resolved to nothing", self.addr))
        })?;
        let mut sock = std::net::TcpStream::connect_timeout(&addr, UPSTREAM_TIMEOUT)?;
        sock.set_read_timeout(Some(UPSTREAM_TIMEOUT))?;
        sock.write_all(&detect::encode(request))?;

        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if let Some((reply, _)) = detect::decode(&buf)? {
                return Ok(detect::as_relay_reply(reply));
            }
            let n = sock.read(&mut chunk)?;
            if n == 0 {
                anyhow::bail!("the printer closed the probe without answering");
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > MAX_FRAME {
                anyhow::bail!("the printer's reply exceeded {MAX_FRAME} bytes");
            }
        }
    }
}

/// Answer from what we were told, for a printer that isn't there.
pub struct SyntheticDetect {
    pub serial: String,
    pub model: String,
}

impl DetectSource for SyntheticDetect {
    fn detect(&self, request: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let seq = detect::detect_sequence_id(request).unwrap_or("0");
        Ok(detect::detect_reply(
            seq,
            &self.serial,
            &self.model,
            "synthetic",
            "00.00.00.00",
        ))
    }
}

/// Serve the detect port until the process ends.
///
/// `tls` is `Some` for the TLS variant of the port (3002 on a real printer),
/// `None` for the plain one (3000). Both are served, because which one a client
/// tries is the client's business — the printer offers both.
pub async fn serve(
    listener: TcpListener,
    source: Arc<dyn DetectSource>,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> anyhow::Result<()> {
    let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
    let in_flight = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT));
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("emulate-detect: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Acquired before spawning, so a flood makes the *listener* wait rather
        // than piling up tasks that would each hold a socket open regardless.
        let permit = match Arc::clone(&in_flight).acquire_owned().await {
            Ok(p) => p,
            Err(_) => return Ok(()), // semaphore closed: we are shutting down
        };
        let source = Arc::clone(&source);
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let outcome = match acceptor {
                Some(acceptor) => match acceptor.accept(socket).await {
                    Ok(stream) => answer_one(stream, source).await,
                    Err(e) => Err(anyhow::anyhow!("TLS handshake failed: {e}")),
                },
                None => answer_one(socket, source).await,
            };
            if let Err(e) = outcome {
                eprintln!("emulate-detect: probe from {peer} failed: {e}");
            }
        });
    }
}

/// One probe: read a frame, answer it, close. The printer does exactly this.
async fn answer_one<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut socket: S,
    source: Arc<dyn DetectSource>,
) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let request = loop {
        if let Some((value, _)) = detect::decode(&buf)? {
            break value;
        }
        let n = match tokio::time::timeout(READ_TIMEOUT, socket.read(&mut chunk)).await {
            Ok(n) => n?,
            Err(_) => anyhow::bail!("no probe within {READ_TIMEOUT:?}"),
        };
        if n == 0 {
            return Ok(()); // connected and thought better of it
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_FRAME {
            anyhow::bail!("probe exceeded {MAX_FRAME} bytes");
        }
    };

    if !detect::is_detect(&request) {
        // Not the enquiry we know. Saying nothing is what an unknown command
        // gets from the printer, and inventing an answer would be worse.
        anyhow::bail!("not a detect enquiry: {request}");
    }

    let reply = tokio::task::spawn_blocking(move || source.detect(&request)).await??;
    socket.write_all(&detect::encode(&reply)).await?;
    socket.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::TcpStream;

    struct Fixed;
    impl DetectSource for Fixed {
        fn detect(&self, request: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
            let seq = detect::detect_sequence_id(request).unwrap_or("0");
            Ok(detect::detect_reply(
                seq,
                "0309FATEST00001",
                "N1",
                "3DP-030-488",
                "01.07.02.00",
            ))
        }
    }

    async fn start() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, Arc::new(Fixed), None));
        addr
    }

    #[tokio::test]
    async fn it_answers_the_probe_studio_actually_sends() {
        // The exact bytes captured from the A1 mini.
        let probe = b"\xa5\xa5\x3a\x00{\"login\":{\"command\":\"detect\",\"sequence_id\":\"20004\"}}\xa7\xa7";
        let addr = start().await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(probe).await.unwrap();

        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        let reply = loop {
            if let Some((v, _)) = detect::decode(&buf).unwrap() {
                break v;
            }
            let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut chunk))
                .await
                .expect("a reply should arrive")
                .unwrap();
            assert_ne!(n, 0, "the relay closed without answering");
            buf.extend_from_slice(&chunk[..n]);
        };
        assert_eq!(reply["login"]["sequence_id"], "20004");
        assert_eq!(reply["login"]["connect"], "lan");
        assert_eq!(reply["login"]["bind"], "free");
    }

    #[tokio::test]
    async fn a_peer_that_connects_and_says_nothing_is_just_dropped() {
        let addr = start().await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.shutdown().await.unwrap();
        let mut buf = Vec::new();
        // Nothing sent back, and no panic on our side.
        let n = tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut buf))
            .await
            .expect("should close promptly")
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn a_frame_that_is_not_a_detect_gets_no_invented_answer() {
        let addr = start().await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&detect::encode(&json!({"login": {"command": "bind"}})))
            .await
            .unwrap();
        let mut buf = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut buf))
            .await
            .expect("should close")
            .unwrap();
        assert_eq!(n, 0, "an unknown command is met with silence, not a guess");
    }
}
