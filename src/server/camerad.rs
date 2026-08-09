//! The emulated printer's camera port — and the substitution that is the point
//! of it.
//!
//! A client that has been told this host is the printer will look for a chamber
//! camera on TCP 6000, authenticate, and expect JPEG frames
//! ([`crate::core::camerad`] has the wire format). This serves that, with the
//! pictures coming from **somewhere else**: the A1 mini here has a dead camera,
//! and an external one pointed at the bed is a better view than the printer's
//! own even when it works.
//!
//! Two things follow from the substitution being deliberate:
//!
//! - it is never implicit. A camera has to be named for this, because
//!   presenting one machine's video as another's is exactly the sort of thing
//!   that should not happen because a URL was lying around in a config.
//! - the port is not served at all when nothing was named, rather than served
//!   emptily. A client then sees no camera, which is the truth.
//!
//! **What a stalled source does.** The connection stays open and goes quiet.
//! That is not invention: it is what this printer's dead camera does — it
//! accepts the connection and never sends a frame — so it is the one failure
//! mode every client that talks to a Bambu printer has already had to survive.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::core::camerad::{self, AUTH_PACKET};
use crate::server::emulate;

/// How long a client has to send its authentication packet.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a peer may take over the TLS handshake, before it has said who it
/// is and while it is holding one of the viewer slots.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// How many viewers at once. Frames are shared, so this bounds sockets, not work.
const MAX_VIEWERS: usize = 16;

/// The latest frame, and a way to wait for the next one.
///
/// A [`tokio::sync::watch`] rather than a queue on purpose: a viewer that stops
/// reading should fall behind by *skipping* frames, not by growing a buffer
/// until it takes the process down with it. Live video has no value in the
/// backlog.
pub type FrameFeed = tokio::sync::watch::Receiver<Option<Arc<Vec<u8>>>>;

/// How long to wait before reopening a source that failed.
const REOPEN_DELAY: Duration = Duration::from_secs(2);

/// How much of a stream to hold while looking for one frame's boundaries.
///
/// A source that never emits the delimiter would otherwise be buffered until
/// the process dies. Two frames' worth of the largest picture our own client
/// will accept is generous and still bounded.
const MAX_BUFFER: usize = 2 * camerad::MAX_FRAME;

/// Keeps [`FrameFeed`] fed from a substituted camera.
///
/// Two shapes of source, because that is what cameras offer: an endless
/// `multipart/x-mixed-replace` stream, or one JPEG per request. The stream is
/// preferred wherever a camera has one — polling a snapshot URL is a worse
/// picture of the same thing.
pub struct FrameSource;

impl FrameSource {
    /// Feed from an endless MJPEG stream, reopening it whenever it ends.
    ///
    /// Returns immediately; frames appear in the feed as they arrive. `open` is
    /// a closure rather than a URL so a test can hand over canned bytes, the
    /// way [`crate::server::stream_record`] does.
    pub fn from_mjpeg(open: crate::server::camera::StreamOpen) -> FrameFeed {
        let (tx, rx) = tokio::sync::watch::channel(None);
        std::thread::spawn(move || {
            while !tx.is_closed() {
                match open() {
                    Ok(stream) => {
                        if let Err(e) = pump_mjpeg(stream, &tx) {
                            eprintln!("emulate-camera: source stream ended: {e}");
                        }
                    }
                    Err(e) => eprintln!("emulate-camera: cannot open the source: {e}"),
                }
                // Viewers stay connected and quiet across this, which is what a
                // printer's camera looks like when it has nothing to send.
                std::thread::sleep(REOPEN_DELAY);
            }
        });
        rx
    }

    /// Feed by asking for one JPEG at a time.
    ///
    /// `every` has no default on purpose: the right rate belongs to the camera
    /// and the network, and a number invented here would be wrong for both.
    pub fn from_snapshots(
        snapshot: Arc<dyn Fn() -> Result<Vec<u8>, String> + Send + Sync>,
        every: Duration,
    ) -> FrameFeed {
        let (tx, rx) = tokio::sync::watch::channel(None);
        std::thread::spawn(move || {
            while !tx.is_closed() {
                match snapshot() {
                    Ok(frame) if camerad::is_jpeg(&frame) => {
                        let _ = tx.send(Some(Arc::new(frame)));
                    }
                    Ok(other) => eprintln!(
                        "emulate-camera: source returned {} bytes that are not a JPEG; skipped",
                        other.len()
                    ),
                    Err(e) => eprintln!("emulate-camera: snapshot failed: {e}"),
                }
                std::thread::sleep(every);
            }
        });
        rx
    }
}

/// Read one multipart stream to its end, publishing each frame.
fn pump_mjpeg(
    stream: crate::server::camera::OpenedCameraStream,
    tx: &tokio::sync::watch::Sender<Option<Arc<Vec<u8>>>>,
) -> Result<(), String> {
    use std::io::Read as _;

    let parts = camerad::Multipart::from_content_type(&stream.content_type).ok_or_else(|| {
        format!(
            "content-type {:?} names no multipart boundary; is this a stream URL?",
            stream.content_type
        )
    })?;
    let mut reader = stream.reader;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        // Drain whatever whole frames the buffer already holds before reading
        // again, or a fast camera would outrun us into unbounded memory.
        loop {
            match parts.next_frame(&buf) {
                Ok(Some((range, used))) => {
                    let frame = &buf[range];
                    if camerad::is_jpeg(frame) {
                        let _ = tx.send(Some(Arc::new(frame.to_vec())));
                    }
                    buf.drain(..used);
                }
                Ok(None) => break,
                Err(e) => return Err(e.to_string()),
            }
        }
        if buf.len() > MAX_BUFFER {
            return Err(format!(
                "no frame boundary in {} bytes; the source is not the stream it claimed",
                buf.len()
            ));
        }
        let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(()); // the stream ended; the caller reopens it
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Where frames come from, and who is allowed to see them.
pub struct CameraRelay {
    access_code: String,
    frames: FrameFeed,
}

impl CameraRelay {
    pub fn new(access_code: &str, frames: FrameFeed) -> Arc<Self> {
        Arc::new(Self {
            access_code: access_code.to_string(),
            frames,
        })
    }

    /// Serve the camera port until the process ends.
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        tls: Arc<rustls::ServerConfig>,
    ) -> anyhow::Result<()> {
        let acceptor = tokio_rustls::TlsAcceptor::from(tls);
        let viewers = Arc::new(tokio::sync::Semaphore::new(MAX_VIEWERS));
        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("emulate-camera: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = Arc::clone(&viewers).try_acquire_owned() else {
                eprintln!("emulate-camera: refusing {peer}: already serving {MAX_VIEWERS} viewers");
                drop(socket);
                continue;
            };
            let relay = Arc::clone(&self);
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let _permit = permit;
                // Bounded like the MQTT side: the permit is taken before the
                // handshake, so peers that connect and then dribble would hold
                // every viewer slot until the process restarted — and it costs
                // them nothing to do it again.
                let accepted = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(socket));
                match accepted.await {
                    Err(_) => {
                        eprintln!(
                            "emulate-camera: {peer} never finished TLS within {HANDSHAKE_TIMEOUT:?}"
                        )
                    }
                    Ok(Ok(stream)) => {
                        // Logged at both ends, and this is not housekeeping:
                        // the server side of this protocol has never been
                        // observed, so *where* a real client gives up is the
                        // only evidence available about which guess was wrong.
                        eprintln!("emulate-camera: {peer} connected");
                        match relay.serve_one(stream).await {
                            Ok(sent) => {
                                eprintln!("emulate-camera: {peer} left after {sent} frame(s)")
                            }
                            // A viewer that takes what it wanted and hangs up
                            // is not a fault — it is what this crate's own
                            // client does, one frame per connection. Calling
                            // that an error buries the failures worth reading
                            // in a log full of normal departures.
                            Err(e) if emulate::is_ordinary_disconnect(&e) => {
                                eprintln!("emulate-camera: {peer} left ({e})")
                            }
                            Err(e) => eprintln!("emulate-camera: {peer} dropped: {e}"),
                        }
                    }
                    Ok(Err(e)) => {
                        eprintln!("emulate-camera: TLS handshake with {peer} failed: {e}")
                    }
                }
            });
        }
    }

    /// One viewer: read the authentication packet, then send frames until it
    /// goes away.
    pub async fn serve_one<S>(&self, mut stream: S) -> anyhow::Result<u64>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let mut buf = Vec::with_capacity(AUTH_PACKET);
        let mut chunk = [0u8; AUTH_PACKET];
        let creds = loop {
            if let Some(creds) = camerad::parse_auth(&buf)? {
                break creds;
            }
            let n = match tokio::time::timeout(AUTH_TIMEOUT, stream.read(&mut chunk)).await {
                Ok(n) => n?,
                Err(_) => anyhow::bail!("no authentication within {AUTH_TIMEOUT:?}"),
            };
            if n == 0 {
                return Ok(0); // connected and thought better of it
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > AUTH_PACKET {
                anyhow::bail!("authentication packet is longer than {AUTH_PACKET} bytes");
            }
        };
        // A mistyped access code and a camera that is simply off look identical
        // from the outside — silence either way. Saying so here is what makes
        // the difference debuggable; the code itself is never printed.
        if creds.access_code != self.access_code {
            anyhow::bail!("wrong access code");
        }

        eprintln!("emulate-camera: authenticated, sending frames");
        let mut feed = self.frames.clone();
        let mut sent = 0u64;
        // Whatever is current goes out first, so a viewer joining mid-stream
        // sees a picture immediately instead of waiting for the next capture.
        let mut pending = feed.borrow_and_update().clone();
        loop {
            if let Some(frame) = pending.take() {
                // The count goes into any failure: the server side of this
                // protocol has never been observed, so "gave up after the
                // header", "after one frame" and "after twenty" are three
                // different diagnoses and the only ones available.
                let write = async {
                    stream
                        .write_all(&camerad::frame_header(frame.len() as u32))
                        .await?;
                    stream.write_all(&frame).await?;
                    stream.flush().await
                };
                if let Err(e) = write.await {
                    return Err(anyhow::Error::new(e)
                        .context(format!("after {sent} frame(s), {} bytes", frame.len())));
                }
                sent += 1;
            }
            // No timeout: a source that has stopped producing leaves the viewer
            // connected and quiet, which is what a real printer's idle camera
            // does. The viewer decides when it has waited long enough.
            if feed.changed().await.is_err() {
                return Ok(sent); // the feed is gone; nothing more will arrive
            }
            pending = feed.borrow_and_update().clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::camerad::FRAME_HEADER;
    use tokio::io::duplex;

    const CODE: &str = "13572468";

    fn jpeg(fill: u8, len: usize) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.extend(std::iter::repeat_n(fill, len));
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    /// Read one framed JPEG the way `crate::camera` does.
    async fn read_frame<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Vec<u8> {
        let mut header = [0u8; FRAME_HEADER];
        stream.read_exact(&mut header).await.expect("a header");
        let len = camerad::frame_len(&header).expect("a sane length");
        let mut frame = vec![0u8; len];
        stream.read_exact(&mut frame).await.expect("the frame");
        frame
    }

    #[tokio::test]
    async fn a_viewer_that_authenticates_is_sent_the_current_picture() {
        let first = jpeg(0x11, 2000);
        let (tx, rx) = tokio::sync::watch::channel(Some(Arc::new(first.clone())));
        let relay = CameraRelay::new(CODE, rx);
        let (mut client, server) = duplex(64 * 1024);
        tokio::spawn(async move { relay.serve_one(server).await });

        client.write_all(&camerad::auth_packet(CODE)).await.unwrap();
        assert_eq!(
            read_frame(&mut client).await,
            first,
            "the frame already held"
        );

        let next = jpeg(0x22, 3000);
        tx.send(Some(Arc::new(next.clone()))).unwrap();
        assert_eq!(read_frame(&mut client).await, next, "and the one after it");
    }

    #[tokio::test]
    async fn two_viewers_see_the_same_frame_from_one_source() {
        // The whole reason the feed is shared: a second viewer must not mean a
        // second camera being opened.
        let (tx, rx) = tokio::sync::watch::channel(None);
        let relay = CameraRelay::new(CODE, rx);
        let mut clients = Vec::new();
        for _ in 0..2 {
            let (mut client, server) = duplex(64 * 1024);
            let relay = Arc::clone(&relay);
            tokio::spawn(async move { relay.serve_one(server).await });
            client.write_all(&camerad::auth_packet(CODE)).await.unwrap();
            clients.push(client);
        }
        let frame = jpeg(0x33, 2500);
        tx.send(Some(Arc::new(frame.clone()))).unwrap();
        for (i, client) in clients.iter_mut().enumerate() {
            assert_eq!(read_frame(client).await, frame, "viewer {i}");
        }
    }

    #[tokio::test]
    async fn the_wrong_access_code_gets_nothing() {
        let (_tx, rx) = tokio::sync::watch::channel(Some(Arc::new(jpeg(0x44, 2000))));
        let relay = CameraRelay::new(CODE, rx);
        let (mut client, server) = duplex(64 * 1024);
        let served = tokio::spawn(async move { relay.serve_one(server).await });

        client
            .write_all(&camerad::auth_packet("00000000"))
            .await
            .unwrap();
        assert!(served.await.unwrap().is_err(), "it should be refused");
        let mut anything = Vec::new();
        client.read_to_end(&mut anything).await.unwrap();
        assert!(
            anything.is_empty(),
            "a refused viewer must not be shown the camera"
        );
    }

    #[tokio::test]
    async fn an_authentication_split_across_reads_still_works() {
        // 80 bytes is small enough to arrive in one packet nearly always, which
        // is exactly why the case that doesn't would go unnoticed.
        let frame = jpeg(0x55, 2000);
        let (_tx, rx) = tokio::sync::watch::channel(Some(Arc::new(frame.clone())));
        let relay = CameraRelay::new(CODE, rx);
        let (mut client, server) = duplex(64 * 1024);
        tokio::spawn(async move { relay.serve_one(server).await });

        let packet = camerad::auth_packet(CODE);
        for byte in &packet {
            client.write_all(&[*byte]).await.unwrap();
            client.flush().await.unwrap();
        }
        assert_eq!(read_frame(&mut client).await, frame);
    }

    fn multipart_of(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in frames {
            out.extend_from_slice(b"--frame\r\nContent-Type: image/jpeg\r\n");
            out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", f.len()).as_bytes());
            out.extend_from_slice(f);
            out.extend_from_slice(b"\r\n");
        }
        out
    }

    /// Watch the feed until it settles on `want`, checking everything seen on
    /// the way.
    ///
    /// The feed keeps only the newest frame — a viewer that blinks misses one,
    /// by design — so a test cannot demand to see every frame in order. What it
    /// can demand is that whatever it *does* see is a whole frame from the
    /// source, and that the newest one arrives.
    async fn settles_on(feed: &mut FrameFeed, want: &[u8], all: &[Vec<u8>]) {
        let found = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(f) = feed.borrow_and_update().clone() {
                    assert!(
                        all.iter().any(|w| w == f.as_ref()),
                        "a frame arrived that is not one the source sent ({} bytes)",
                        f.len()
                    );
                    if f.as_slice() == want {
                        return;
                    }
                }
                if feed.changed().await.is_err() {
                    panic!("the feed closed before the frame arrived");
                }
            }
        })
        .await;
        assert!(found.is_ok(), "the newest frame never reached the feed");
    }

    #[tokio::test]
    async fn an_mjpeg_source_becomes_a_feed_of_whole_frames() {
        let want = vec![jpeg(0xA1, 1500), jpeg(0xA2, 1600)];
        let body = multipart_of(&want);
        let open: crate::server::camera::StreamOpen = Arc::new(move || {
            Ok(crate::server::camera::OpenedCameraStream {
                content_type: "multipart/x-mixed-replace; boundary=frame".into(),
                reader: Box::new(std::io::Cursor::new(body.clone())),
            })
        });
        let mut feed = FrameSource::from_mjpeg(open);
        settles_on(&mut feed, &want[1], &want).await;
    }

    #[tokio::test]
    async fn a_source_serving_an_error_page_publishes_nothing() {
        // An HTTP camera answers with HTML when it is unhappy, and a wall of
        // HTML framed as a photograph is worse than no picture.
        let html = b"<html>502 Bad Gateway</html>".repeat(60);
        let body = multipart_of(&[html]);
        let open: crate::server::camera::StreamOpen = Arc::new(move || {
            Ok(crate::server::camera::OpenedCameraStream {
                content_type: "multipart/x-mixed-replace; boundary=frame".into(),
                reader: Box::new(std::io::Cursor::new(body.clone())),
            })
        });
        let mut feed = FrameSource::from_mjpeg(open);
        let got = tokio::time::timeout(Duration::from_millis(400), feed.changed()).await;
        assert!(got.is_err(), "nothing should have been published");
    }

    #[tokio::test]
    async fn a_snapshot_source_is_polled_at_the_rate_it_was_given() {
        let frame = jpeg(0xB1, 1200);
        let want = frame.clone();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let mut feed = FrameSource::from_snapshots(
            Arc::new(move || {
                seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(frame.clone())
            }),
            Duration::from_millis(50),
        );
        settles_on(&mut feed, &want, std::slice::from_ref(&want)).await;
        assert!(calls.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    }

    /// The witness test.
    ///
    /// Everything else here checks this server against expectations written
    /// alongside it, which is worth little when the real server has never been
    /// observed. `crate::camera::CameraClient` is different: it was written
    /// against a real printer, it runs against real hardware in production, and
    /// it knows nothing about this module. If the emulated side satisfies it —
    /// over real TLS, with the real 80-byte packet, through the real framing —
    /// then the shapes agree with something this code did not get to shape.
    #[tokio::test]
    async fn the_production_client_is_satisfied_by_the_emulated_camera() {
        let frame = jpeg(0x77, 4096);
        let (_tx, rx) = tokio::sync::watch::channel(Some(Arc::new(frame.clone())));
        let relay = CameraRelay::new(CODE, rx);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = crate::tls::emulated_printer_server_config("0309FATEST00001", None).unwrap();
        tokio::spawn(relay.serve(listener, tls));

        let target = crate::config::ResolvedTarget {
            camera_port: addr.port(),
            ..crate::config::ResolvedTarget::new(
                "127.0.0.1",
                "0309FATEST00001",
                CODE,
                crate::core::model::Model::A1Mini,
            )
        };
        // Blocking client, so off the runtime's worker threads.
        let got = tokio::task::spawn_blocking(move || {
            crate::camera::CameraClient::new(target)
                .with_timeout(Duration::from_secs(10))
                .snapshot()
        })
        .await
        .unwrap()
        .expect("the production client should get a frame");

        assert_eq!(got, frame, "byte for byte, through the real client");
    }

    #[tokio::test]
    async fn a_source_that_stops_leaves_the_viewer_connected_and_quiet() {
        // What this printer's dead camera does: accepts, then nothing. Hanging
        // up instead would be behaviour no Bambu printer has been seen to have.
        let frame = jpeg(0x66, 2000);
        let (_tx, rx) = tokio::sync::watch::channel(Some(Arc::new(frame.clone())));
        let relay = CameraRelay::new(CODE, rx);
        let (mut client, server) = duplex(64 * 1024);
        tokio::spawn(async move { relay.serve_one(server).await });

        client.write_all(&camerad::auth_packet(CODE)).await.unwrap();
        assert_eq!(read_frame(&mut client).await, frame);

        // Nothing more is produced; the connection must not be closed under us.
        let mut byte = [0u8; 1];
        let idle = tokio::time::timeout(Duration::from_millis(300), client.read(&mut byte)).await;
        assert!(idle.is_err(), "it went quiet, it did not hang up");
    }
}
