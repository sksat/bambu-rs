//! The emulated printer's FTP server — store-and-forward to the real one.
//!
//! Bambu Studio sends a print in two steps: upload the sliced `.gcode.3mf` over
//! implicit FTPS on 990, then `print.project_file` naming what it uploaded. With
//! the MQTT relay alone a client can watch and control a print but not start
//! one, because its upload goes to whichever host it was pointed at — this one.
//!
//! So an upload is taken here in full, then pushed to the printer with the same
//! [`FtpsClient`](crate::ftp::FtpsClient) the CLI uses. **Store and forward, not
//! streaming**: the file lands in a temp file before a byte of it goes on, which
//! costs the transfer twice over but means a client that dies mid-upload leaves
//! nothing half-written on the printer's SD card — and that card is the one part
//! of this machine already known to be fragile (`docs/protocol.md`).
//!
//! The command grammar and every reply code live in [`crate::core::ftpd`], where
//! they are tested without a socket. Here: TLS, data connections, and the
//! forwarding itself.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::core::ftpd::{FtpAction, FtpSession, PassiveKind, Reply};
use crate::ftp::{FileEntry, FtpsClient};

/// The user every Bambu client logs in as.
const FTP_USER: &str = "bblp";

/// How long a client has to log in before we hang up. It is unauthenticated for
/// all of it, so the window is short.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a data connection may take to arrive after a `PASV`.
const DATA_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest a single control line may be. A real one is a path; a megabyte of
/// bytes with no newline is someone probing for a buffer to overrun.
const MAX_LINE: usize = 8 * 1024;

/// Largest upload accepted, before it is forwarded. Generous next to a sliced
/// `.3mf` (tens of MB) and far below anything that would fill a disk.
const MAX_UPLOAD: u64 = 512 * 1024 * 1024;

/// The printer's file store, as the relay needs it.
///
/// Deliberately not [`FileStore`](super::files::FileStore): that one serves the
/// dashboard, so its `fetch` is capped for a browser and it has no delete. The
/// two want different things from the same FTP client, and widening the
/// dashboard's trait to fit this would give the dashboard a delete it should
/// not have.
pub trait PrinterFiles: Send + Sync + 'static {
    /// The printer's own `LIST` lines, verbatim.
    fn list_raw(&self, dir: &str) -> Result<Vec<String>, String>;
    /// File names in `dir` (`NLST`).
    fn list_names(&self, dir: &str) -> Result<Vec<String>, String>;
    /// Parsed entries — used for `SIZE`, which has no direct FTP client call.
    fn entries(&self, dir: &str) -> Result<Vec<FileEntry>, String>;
    fn upload(&self, local: &Path, remote: &str) -> Result<u64, String>;
    fn download(&self, remote: &str, local: &Path) -> Result<u64, String>;
    fn delete(&self, remote: &str) -> Result<(), String>;
    /// Uploading to a temp name and renaming into place is a common client
    /// idiom, and the printer supports it — so the relay must too, or it breaks
    /// a flow the printer alone would have satisfied.
    fn rename(&self, from: &str, to: &str) -> Result<(), String>;
    fn mkdir(&self, path: &str) -> Result<(), String>;
    fn rmdir(&self, path: &str) -> Result<(), String>;
}

/// How long to spend finding out whether the printer is answering at all.
///
/// `suppaftp`'s implicit-TLS connect has no timeout, so a printer that is off or
/// has moved to a new DHCP lease costs the *client* the OS TCP timeout — a
/// couple of minutes of a progress bar that is never going to finish. One cheap
/// TCP probe first turns that into an immediate, accurate refusal, and it costs
/// a handshake on a LAN when the printer is up.
const UPSTREAM_PROBE: Duration = Duration::from_secs(5);

/// Most control connections served at once — the same reasoning as the MQTT
/// listener's cap, and the same pre-auth exposure.
const MAX_CONNECTIONS: usize = 64;

/// [`PrinterFiles`] backed by the real printer over FTPS.
pub struct LivePrinterFiles {
    client: FtpsClient,
    addr: String,
}

impl LivePrinterFiles {
    pub fn new(target: crate::config::ResolvedTarget) -> Self {
        let addr = format!("{}:{}", target.ip, crate::ftp::FTPS_PORT);
        Self {
            client: FtpsClient::new(target),
            addr,
        }
    }

    /// Fail fast if the printer isn't answering — see [`UPSTREAM_PROBE`].
    fn reachable(&self) -> Result<(), String> {
        use std::net::ToSocketAddrs;
        let mut addrs = self
            .addr
            .to_socket_addrs()
            .map_err(|e| format!("cannot resolve {}: {e}", self.addr))?;
        let addr = addrs
            .next()
            .ok_or_else(|| format!("{} resolved to nothing", self.addr))?;
        std::net::TcpStream::connect_timeout(&addr, UPSTREAM_PROBE)
            .map(|_| ())
            .map_err(|e| format!("the printer is not answering on {}: {e}", self.addr))
    }
}

impl PrinterFiles for LivePrinterFiles {
    fn list_raw(&self, dir: &str) -> Result<Vec<String>, String> {
        self.reachable()?;
        self.client.list_raw(dir).map_err(|e| e.to_string())
    }
    fn list_names(&self, dir: &str) -> Result<Vec<String>, String> {
        self.reachable()?;
        self.client.list(dir).map_err(|e| e.to_string())
    }
    fn entries(&self, dir: &str) -> Result<Vec<FileEntry>, String> {
        self.reachable()?;
        self.client.list_entries(dir).map_err(|e| e.to_string())
    }
    fn upload(&self, local: &Path, remote: &str) -> Result<u64, String> {
        self.reachable()?;
        self.client.upload(local, remote).map_err(|e| e.to_string())
    }
    fn download(&self, remote: &str, local: &Path) -> Result<u64, String> {
        self.reachable()?;
        self.client
            .download(remote, local)
            .map_err(|e| e.to_string())
    }
    fn delete(&self, remote: &str) -> Result<(), String> {
        self.reachable()?;
        self.client.delete(remote).map_err(|e| e.to_string())
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.reachable()?;
        self.client.rename(from, to).map_err(|e| e.to_string())
    }
    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.reachable()?;
        self.client.mkdir(path).map_err(|e| e.to_string())
    }
    fn rmdir(&self, path: &str) -> Result<(), String> {
        self.reachable()?;
        self.client.rmdir(path).map_err(|e| e.to_string())
    }
}

/// The emulated printer's FTP listener.
pub struct FtpRelay {
    access_code: String,
    files: Arc<dyn PrinterFiles>,
    /// Refuse anything that writes. Mirrors the MQTT relay's read-only mode:
    /// there, control; here, uploads and deletes.
    read_only: bool,
}

impl FtpRelay {
    pub fn new(access_code: impl Into<String>, files: Arc<dyn PrinterFiles>) -> Arc<Self> {
        Arc::new(Self {
            access_code: access_code.into(),
            files,
            read_only: false,
        })
    }

    pub fn read_only(access_code: impl Into<String>, files: Arc<dyn PrinterFiles>) -> Arc<Self> {
        Arc::new(Self {
            access_code: access_code.into(),
            files,
            read_only: true,
        })
    }

    /// Accept control connections forever, each in its own task.
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        tls: Arc<rustls::ServerConfig>,
    ) -> anyhow::Result<()> {
        let acceptor = tokio_rustls::TlsAcceptor::from(tls.clone());
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("emulate-ftp: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
                eprintln!(
                    "emulate-ftp: refusing {peer}: already serving {MAX_CONNECTIONS} clients"
                );
                drop(socket);
                continue;
            };
            let local = socket.local_addr().ok();
            let acceptor = acceptor.clone();
            let tls = tls.clone();
            let this = Arc::clone(&self);
            tokio::spawn(async move {
                let _slot = slot;
                // Implicit FTPS: TLS starts immediately, no AUTH TLS step.
                let stream =
                    match tokio::time::timeout(LOGIN_TIMEOUT, acceptor.accept(socket)).await {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            eprintln!("emulate-ftp: TLS handshake with {peer} failed: {e}");
                            return;
                        }
                        Err(_) => return,
                    };
                if let Err(e) = this.serve_control(stream, local, Some(peer), tls).await {
                    eprintln!("emulate-ftp: {peer} dropped: {e}");
                }
            });
        }
    }

    /// Drive one control connection to its end.
    async fn serve_control<S>(
        self: Arc<Self>,
        mut stream: S,
        local: Option<SocketAddr>,
        peer: Option<SocketAddr>,
        tls: Arc<rustls::ServerConfig>,
    ) -> anyhow::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
    {
        let mut session = FtpSession::new(FTP_USER, &self.access_code);
        write_reply(&mut stream, &FtpSession::greeting()).await?;

        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        // Held between PASV and the transfer command that uses it.
        let mut data_listener: Option<TcpListener> = None;
        let acceptor = tokio_rustls::TlsAcceptor::from(tls);
        let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;

        loop {
            // Only the login is time-boxed: once authenticated, a client may sit
            // on an idle control connection between transfers as long as it likes,
            // which is what an FTP client does while the user thinks.
            let read = if session.is_authenticated() {
                stream.read(&mut chunk).await?
            } else {
                match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
                    Ok(n) => n?,
                    Err(_) => anyhow::bail!("no login within {LOGIN_TIMEOUT:?}"),
                }
            };
            if read == 0 {
                return Ok(()); // client hung up
            }
            buf.extend_from_slice(&chunk[..read]);

            // A read can carry several commands, or part of one.
            while let Some(end) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line).trim_end().to_string();
                if line.is_empty() {
                    continue;
                }
                let action = session.handle(&line);
                if self
                    .run(
                        action,
                        &mut stream,
                        &mut data_listener,
                        local,
                        peer,
                        &acceptor,
                    )
                    .await?
                {
                    return Ok(()); // the action was a close
                }
            }
            // Checked on what is LEFT once the complete lines are gone: a client
            // pipelining more than the cap in one segment is legitimate, a
            // client sending bytes that never contain a newline is not.
            if buf.len() > MAX_LINE {
                anyhow::bail!(
                    "client is holding {} bytes with no end of line in them",
                    buf.len()
                );
            }
        }
    }

    /// Carry out one action. Returns `true` when the connection should close.
    async fn run<S>(
        &self,
        action: FtpAction,
        stream: &mut S,
        data_listener: &mut Option<TcpListener>,
        local: Option<SocketAddr>,
        peer: Option<SocketAddr>,
        acceptor: &tokio_rustls::TlsAcceptor,
    ) -> anyhow::Result<bool>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
    {
        match action {
            FtpAction::Reply(reply) => write_reply(stream, &reply).await?,
            FtpAction::Close(reply) => {
                write_reply(stream, &reply).await?;
                return Ok(true);
            }
            FtpAction::Passive(kind) => {
                // Bound to the interface the client already reached us on, so a
                // relay listening on one address can't hand out another.
                // A SocketAddr, not a formatted string: `format!("{}:0", ip)`
                // turns an IPv6 address into `::1:0`, which parses as nothing.
                let bind = local.map_or_else(
                    || SocketAddr::from(([0, 0, 0, 0], 0)),
                    |a| SocketAddr::new(a.ip(), 0),
                );
                match TcpListener::bind(bind).await {
                    Ok(listener) => {
                        let port = listener.local_addr()?.port();
                        let reply = match kind {
                            PassiveKind::Extended => Reply::new(
                                229,
                                format!("entering extended passive mode (|||{port}|)"),
                            ),
                            PassiveKind::Classic => {
                                let ip = match local.map(|a| a.ip()) {
                                    Some(std::net::IpAddr::V4(v4)) => v4,
                                    // PASV can only express IPv4. A client on
                                    // IPv6 has EPSV and should have used it.
                                    _ => {
                                        write_reply(
                                            stream,
                                            &Reply::new(
                                                522,
                                                "use EPSV: PASV cannot express this address",
                                            ),
                                        )
                                        .await?;
                                        return Ok(false);
                                    }
                                };
                                let o = ip.octets();
                                Reply::new(
                                    227,
                                    format!(
                                        "entering passive mode ({},{},{},{},{},{})",
                                        o[0],
                                        o[1],
                                        o[2],
                                        o[3],
                                        port >> 8,
                                        port & 0xFF
                                    ),
                                )
                            }
                        };
                        *data_listener = Some(listener);
                        write_reply(stream, &reply).await?;
                    }
                    Err(e) => {
                        write_reply(
                            stream,
                            &Reply::new(425, format!("cannot open a data port: {e}")),
                        )
                        .await?
                    }
                }
            }

            FtpAction::Store { path } => {
                if self.read_only {
                    write_reply(
                        stream,
                        &Reply::new(550, "refused by the bambu-rs emulator: relay is read-only"),
                    )
                    .await?;
                    return Ok(false);
                }
                let reply = self
                    .store(stream, data_listener, acceptor, peer, &path)
                    .await
                    .unwrap_or_else(|e| match e {
                        // The one place a terminal reply is written, whichever
                        // way the transfer ended.
                        DataError::Refused(reply) => reply,
                        DataError::Io(e) => Reply::new(550, format!("upload failed: {e}")),
                    });
                write_reply(stream, &reply).await?;
            }

            FtpAction::Retrieve { path } => {
                let reply = self
                    .retrieve(stream, data_listener, acceptor, peer, &path)
                    .await
                    .unwrap_or_else(|e| match e {
                        // The one place a terminal reply is written, whichever
                        // way the transfer ended.
                        DataError::Refused(reply) => reply,
                        DataError::Io(e) => Reply::new(550, format!("download failed: {e}")),
                    });
                write_reply(stream, &reply).await?;
            }

            FtpAction::List { path, names_only } => {
                let reply = self
                    .list(stream, data_listener, acceptor, peer, &path, names_only)
                    .await
                    .unwrap_or_else(|e| match e {
                        // The one place a terminal reply is written, whichever
                        // way the transfer ended.
                        DataError::Refused(reply) => reply,
                        DataError::Io(e) => Reply::new(550, format!("listing failed: {e}")),
                    });
                write_reply(stream, &reply).await?;
            }

            FtpAction::Delete { path } => {
                if self.read_only {
                    write_reply(
                        stream,
                        &Reply::new(550, "refused by the bambu-rs emulator: relay is read-only"),
                    )
                    .await?;
                    return Ok(false);
                }
                let files = Arc::clone(&self.files);
                let p = path.clone();
                let done = tokio::task::spawn_blocking(move || files.delete(&p)).await?;
                let reply = match done {
                    Ok(()) => Reply::new(250, format!("deleted {path}")),
                    Err(e) => Reply::new(550, format!("delete failed: {e}")),
                };
                write_reply(stream, &reply).await?;
            }

            FtpAction::Rename { from, to } => {
                let reply = self
                    .mutate(move |files| files.rename(&from, &to), "renamed")
                    .await?;
                write_reply(stream, &reply).await?;
            }

            FtpAction::MakeDir { path } => {
                let shown = path.clone();
                let reply = self
                    .mutate(move |files| files.mkdir(&path), "created")
                    .await?;
                let reply = match reply.code {
                    250 => Reply::new(257, format!("\"{shown}\" created")),
                    _ => reply,
                };
                write_reply(stream, &reply).await?;
            }

            FtpAction::RemoveDir { path } => {
                let reply = self
                    .mutate(move |files| files.rmdir(&path), "removed")
                    .await?;
                write_reply(stream, &reply).await?;
            }

            FtpAction::Size { path } => {
                let reply = match self.size_of(&path).await {
                    Ok(Some(n)) => Reply::new(213, n.to_string()),
                    Ok(None) => Reply::new(550, format!("no such file: {path}")),
                    Err(e) => Reply::new(550, format!("size failed: {e}")),
                };
                write_reply(stream, &reply).await?;
            }
        }
        Ok(false)
    }

    /// Run one write against the printer off the runtime's worker threads,
    /// honouring the read-only policy. `did` is the past-tense verb for the
    /// success reply.
    async fn mutate<F>(&self, op: F, did: &str) -> anyhow::Result<Reply>
    where
        F: FnOnce(Arc<dyn PrinterFiles>) -> Result<(), String> + Send + 'static,
    {
        if self.read_only {
            return Ok(Reply::new(
                550,
                "refused by the bambu-rs emulator: relay is read-only",
            ));
        }
        let files = Arc::clone(&self.files);
        Ok(
            match tokio::task::spawn_blocking(move || op(files)).await? {
                Ok(()) => Reply::new(250, did.to_string()),
                Err(e) => Reply::new(550, format!("the printer refused: {e}")),
            },
        )
    }

    /// Take an upload in full, then push it to the printer.
    async fn store<S>(
        &self,
        control: &mut S,
        data_listener: &mut Option<TcpListener>,
        acceptor: &tokio_rustls::TlsAcceptor,
        peer: Option<SocketAddr>,
        path: &str,
    ) -> Result<Reply, DataError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
    {
        let mut data = open_data(control, data_listener, acceptor, peer, "sending file").await?;
        // A temp file, not memory: a sliced plate is tens of megabytes and
        // several clients could be uploading at once.
        let staged = tempfile::NamedTempFile::new()?;
        let mut out = tokio::fs::File::from_std(staged.reopen()?);
        let mut total: u64 = 0;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = data.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > MAX_UPLOAD {
                return Ok(Reply::new(
                    552,
                    format!("upload exceeds the {MAX_UPLOAD}-byte limit"),
                ));
            }
            out.write_all(&chunk[..n]).await?;
        }
        out.flush().await?;
        drop(out);
        // Close the TLS session properly rather than just dropping the socket.
        // A client that finishes an upload by unwrapping its TLS layer — which
        // Python's ftplib does, and it is the correct thing to do — waits for
        // our close_notify; dropping the connection instead gives it an
        // "unexpected EOF in violation of protocol" on an upload that worked.
        data.shutdown().await?;
        drop(data);

        let files = Arc::clone(&self.files);
        let remote = path.to_string();
        let local = staged.path().to_path_buf();
        // Blocking FTPS to the printer, off the runtime's worker threads.
        let sent = tokio::task::spawn_blocking(move || files.upload(&local, &remote)).await?;
        match sent {
            Ok(n) => Ok(Reply::new(
                226,
                format!("stored {n} bytes to {path} on the printer"),
            )),
            // The client is told the upload failed and where: it uploaded to us
            // successfully, and the printer is the half that refused.
            Err(e) => Ok(Reply::new(
                550,
                format!("received {total} bytes but the printer refused them: {e}"),
            )),
        }
    }

    async fn retrieve<S>(
        &self,
        control: &mut S,
        data_listener: &mut Option<TcpListener>,
        acceptor: &tokio_rustls::TlsAcceptor,
        peer: Option<SocketAddr>,
        path: &str,
    ) -> Result<Reply, DataError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
    {
        // Fetch before opening the data channel: a missing file should be a 550
        // with nothing transferred, not an empty "successful" download.
        let files = Arc::clone(&self.files);
        let staged = tempfile::NamedTempFile::new()?;
        let local = staged.path().to_path_buf();
        let remote = path.to_string();
        let fetched = tokio::task::spawn_blocking(move || files.download(&remote, &local)).await?;
        if let Err(e) = fetched {
            return Ok(Reply::new(550, format!("the printer refused: {e}")));
        }

        let mut data = open_data(control, data_listener, acceptor, peer, "sending file").await?;
        let mut file = tokio::fs::File::open(staged.path()).await?;
        let sent = tokio::io::copy(&mut file, &mut data).await?;
        data.shutdown().await?;
        Ok(Reply::new(226, format!("sent {sent} bytes")))
    }

    async fn list<S>(
        &self,
        control: &mut S,
        data_listener: &mut Option<TcpListener>,
        acceptor: &tokio_rustls::TlsAcceptor,
        peer: Option<SocketAddr>,
        path: &str,
        names_only: bool,
    ) -> Result<Reply, DataError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
    {
        let files = Arc::clone(&self.files);
        let dir = path.to_string();
        let listed = tokio::task::spawn_blocking(move || {
            if names_only {
                files.list_names(&dir)
            } else {
                // The printer's own lines, verbatim — see FtpsClient::list_raw.
                files.list_raw(&dir)
            }
        })
        .await?;
        let lines = match listed {
            Ok(l) => l,
            Err(e) => return Ok(Reply::new(550, format!("the printer refused: {e}"))),
        };

        let mut data = open_data(control, data_listener, acceptor, peer, "sending listing").await?;
        let body: String = lines.iter().map(|l| format!("{l}\r\n")).collect();
        data.write_all(body.as_bytes()).await?;
        data.shutdown().await?;
        Ok(Reply::new(226, format!("listed {} entries", lines.len())))
    }

    /// `SIZE` has no direct call on the FTP client, so it comes out of the
    /// parent directory's listing.
    async fn size_of(&self, path: &str) -> anyhow::Result<Option<u64>> {
        let (dir, name) = match path.rsplit_once('/') {
            Some(("", name)) => ("/".to_string(), name.to_string()),
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => ("/".to_string(), path.to_string()),
        };
        let files = Arc::clone(&self.files);
        let entries = tokio::task::spawn_blocking(move || files.entries(&dir)).await?;
        let entries = entries.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(entries
            .into_iter()
            .find(|e| e.name == name && !e.is_dir)
            .map(|e| e.size))
    }
}

/// Announce the transfer on the control channel, then accept and TLS-wrap the
/// data connection the client opened after `PASV`.
async fn open_data<S>(
    control: &mut S,
    data_listener: &mut Option<TcpListener>,
    acceptor: &tokio_rustls::TlsAcceptor,
    control_peer: Option<SocketAddr>,
    what: &str,
) -> Result<tokio_rustls::server::TlsStream<tokio::net::TcpStream>, DataError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    let Some(listener) = data_listener.take() else {
        return Err(DataError::Refused(Reply::new(
            425,
            "use PASV or EPSV first",
        )));
    };
    write_reply(control, &Reply::new(150, what)).await?;
    // The data port is open to the whole network for the moment it exists, and
    // whoever connects first gets it. Unchecked, another host on the LAN can
    // race the client to it — winning a RETR hands them the file, and winning a
    // STOR lets them choose the bytes this relay then uploads to a machine that
    // heats and moves. No access code required, just presence and timing. So the
    // data connection must come from the same host as the control connection,
    // which is what every serious FTP server enforces.
    //
    // Dropping a stranger is only half of it: if losing the race also failed the
    // client's transfer, winning it repeatedly would be an unauthenticated
    // denial of service — every STOR and RETR answering 425. So keep accepting
    // until the right host turns up, bounded by one deadline for the whole wait
    // so an attacker cannot extend it either.
    let deadline = tokio::time::Instant::now() + DATA_ACCEPT_TIMEOUT;
    let mut turned_away = 0u32;
    let socket = loop {
        let (socket, from) = match tokio::time::timeout_at(deadline, listener.accept()).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(DataError::Refused(Reply::new(425, format!("accept: {e}")))),
            Err(_) => {
                return Err(DataError::Refused(Reply::new(
                    425,
                    if turned_away > 0 {
                        format!(
                            "no data connection from the control host \
                             ({turned_away} from elsewhere were refused)"
                        )
                    } else {
                        "the data connection was never opened".to_string()
                    },
                )));
            }
        };
        match control_peer.map(|a| a.ip()) {
            Some(expected) if from.ip() != expected => {
                drop(socket);
                turned_away += 1;
                // Once, not per attempt: a flood should not become a log flood.
                if turned_away == 1 {
                    eprintln!(
                        "emulate-ftp: dropping a data connection from {} — control is {expected}; \
                         still waiting for the right host",
                        from.ip()
                    );
                }
            }
            _ => break socket,
        }
    };
    // Implicit FTPS encrypts the data channel too.
    Ok(acceptor.accept(socket).await?)
}

/// Why a data transfer could not start.
///
/// [`DataError::Refused`] carries the reply the *caller* must write, and is the
/// reason `open_data` writes none itself: it used to send a 425 and then return
/// an error the caller turned into a 550, leaving two replies on a control
/// stream that expects exactly one. The next command then read the stale one and
/// every reply after that was off by one.
#[derive(Debug)]
enum DataError {
    Refused(Reply),
    Io(std::io::Error),
}

impl From<std::io::Error> for DataError {
    fn from(e: std::io::Error) -> Self {
        DataError::Io(e)
    }
}

impl From<tokio::task::JoinError> for DataError {
    fn from(e: tokio::task::JoinError) -> Self {
        DataError::Io(std::io::Error::other(e.to_string()))
    }
}

impl From<anyhow::Error> for DataError {
    fn from(e: anyhow::Error) -> Self {
        DataError::Io(std::io::Error::other(e.to_string()))
    }
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataError::Refused(r) => write!(f, "{} {}", r.code, r.text),
            DataError::Io(e) => write!(f, "{e}"),
        }
    }
}

async fn write_reply<S>(stream: &mut S, reply: &Reply) -> anyhow::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    stream.write_all(reply.to_line().as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const SERIAL: &str = "0309FATESTSERIAL";
    const CODE: &str = "12345678";

    /// A printer whose file store lives in memory.
    #[derive(Default)]
    struct FakeFiles {
        uploaded: Mutex<Vec<(String, Vec<u8>)>>,
        stored: Mutex<Vec<(String, Vec<u8>)>>,
        renamed: Mutex<Vec<(String, String)>>,
        dirs: Mutex<Vec<String>>,
        fail_upload: bool,
    }

    impl FakeFiles {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
        fn refusing() -> Arc<Self> {
            Arc::new(Self {
                fail_upload: true,
                ..Default::default()
            })
        }
        fn with_file(name: &str, body: &[u8]) -> Arc<Self> {
            let f = Self::default();
            f.stored
                .lock()
                .unwrap()
                .push((name.to_string(), body.to_vec()));
            Arc::new(f)
        }
        fn uploads(&self) -> Vec<(String, Vec<u8>)> {
            self.uploaded.lock().unwrap().clone()
        }
        fn renames(&self) -> Vec<(String, String)> {
            self.renamed.lock().unwrap().clone()
        }
        fn dirs(&self) -> Vec<String> {
            self.dirs.lock().unwrap().clone()
        }
    }

    impl PrinterFiles for FakeFiles {
        fn list_raw(&self, _dir: &str) -> Result<Vec<String>, String> {
            Ok(vec![
                "-rw-r--r-- 1 root root 92763 Jun 13 18:11 a.gcode.3mf".to_string(),
            ])
        }
        fn list_names(&self, _dir: &str) -> Result<Vec<String>, String> {
            Ok(vec!["a.gcode.3mf".to_string()])
        }
        fn entries(&self, _dir: &str) -> Result<Vec<FileEntry>, String> {
            Ok(vec![FileEntry {
                name: "a.gcode.3mf".to_string(),
                is_dir: false,
                size: 92763,
            }])
        }
        fn upload(&self, local: &Path, remote: &str) -> Result<u64, String> {
            if self.fail_upload {
                return Err("0x0500C010: the SD card said no".to_string());
            }
            let body = std::fs::read(local).map_err(|e| e.to_string())?;
            let n = body.len() as u64;
            self.uploaded
                .lock()
                .unwrap()
                .push((remote.to_string(), body));
            Ok(n)
        }
        fn download(&self, remote: &str, local: &Path) -> Result<u64, String> {
            let stored = self.stored.lock().unwrap();
            let Some((_, body)) = stored.iter().find(|(n, _)| n == remote) else {
                return Err(format!("no such file: {remote}"));
            };
            std::fs::write(local, body).map_err(|e| e.to_string())?;
            Ok(body.len() as u64)
        }
        fn delete(&self, remote: &str) -> Result<(), String> {
            self.stored.lock().unwrap().retain(|(n, _)| n != remote);
            Ok(())
        }
        fn rename(&self, from: &str, to: &str) -> Result<(), String> {
            self.renamed
                .lock()
                .unwrap()
                .push((from.to_string(), to.to_string()));
            Ok(())
        }
        fn mkdir(&self, path: &str) -> Result<(), String> {
            self.dirs.lock().unwrap().push(format!("mkdir {path}"));
            Ok(())
        }
        fn rmdir(&self, path: &str) -> Result<(), String> {
            self.dirs.lock().unwrap().push(format!("rmdir {path}"));
            Ok(())
        }
    }

    async fn start(relay: Arc<FtpRelay>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = crate::tls::emulated_printer_server_config(SERIAL).unwrap();
        tokio::spawn(relay.serve(listener, tls));
        addr
    }

    /// A minimal implicit-FTPS client: TLS from the first byte, CRLF commands.
    struct TestClient {
        stream: tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
        buf: Vec<u8>,
        addr: SocketAddr,
    }

    impl TestClient {
        async fn connect(addr: SocketAddr) -> Self {
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let connector =
                tokio_rustls::TlsConnector::from(crate::tls::lan_client_config().unwrap());
            let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
            let stream = connector.connect(name, tcp).await.unwrap();
            let mut c = Self {
                stream,
                buf: Vec::new(),
                addr,
            };
            assert_eq!(c.read_reply().await.0, 220, "greeting");
            c
        }

        async fn read_reply(&mut self) -> (u16, String) {
            let mut chunk = [0u8; 1024];
            loop {
                if let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = self.buf.drain(..=end).collect();
                    let line = String::from_utf8_lossy(&line).trim_end().to_string();
                    let code = line[..3].parse().unwrap_or(0);
                    return (code, line);
                }
                let n = tokio::time::timeout(Duration::from_secs(10), self.stream.read(&mut chunk))
                    .await
                    .expect("timed out waiting for a reply")
                    .unwrap();
                assert_ne!(n, 0, "the relay closed the connection");
                self.buf.extend_from_slice(&chunk[..n]);
            }
        }

        async fn cmd(&mut self, line: &str) -> (u16, String) {
            self.stream
                .write_all(format!("{line}\r\n").as_bytes())
                .await
                .unwrap();
            self.stream.flush().await.unwrap();
            self.read_reply().await
        }

        async fn login(&mut self) {
            assert_eq!(self.cmd("USER bblp").await.0, 331);
            assert_eq!(self.cmd(&format!("PASS {CODE}")).await.0, 230);
        }

        /// `PASV`, returning the data port the relay opened.
        async fn pasv(&mut self) -> u16 {
            let (code, line) = self.cmd("PASV").await;
            assert_eq!(code, 227, "{line}");
            let nums: Vec<u16> = line
                .rsplit_once('(')
                .unwrap()
                .1
                .trim_end_matches(')')
                .split(',')
                .map(|n| n.trim().parse().unwrap())
                .collect();
            (nums[4] << 8) | nums[5]
        }

        async fn data_connect(
            &self,
            port: u16,
        ) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
            let tcp = tokio::net::TcpStream::connect((self.addr.ip(), port))
                .await
                .unwrap();
            let connector =
                tokio_rustls::TlsConnector::from(crate::tls::lan_client_config().unwrap());
            let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
            connector.connect(name, tcp).await.unwrap()
        }

        /// Send a command that opens a data transfer, and push `body` into it.
        async fn upload(&mut self, path: &str, body: &[u8]) -> (u16, String) {
            let port = self.pasv().await;
            self.stream
                .write_all(format!("STOR {path}\r\n").as_bytes())
                .await
                .unwrap();
            self.stream.flush().await.unwrap();
            let mut data = self.data_connect(port).await;
            assert_eq!(self.read_reply().await.0, 150, "transfer should start");
            data.write_all(body).await.unwrap();
            data.shutdown().await.unwrap();
            // The relay must close its TLS session properly, not just drop the
            // socket: a client that unwraps its TLS layer after an upload (as
            // Python's ftplib does) otherwise gets "unexpected EOF in violation
            // of protocol" on a transfer that actually worked. A clean
            // close_notify reads back as EOF; a dropped socket reads as an error.
            let mut rest = Vec::new();
            let closed = data.read_to_end(&mut rest).await;
            assert!(
                closed.is_ok(),
                "the data connection should close cleanly, got {closed:?}"
            );
            drop(data);
            self.read_reply().await
        }

        /// Send a command that returns data, and read the whole data channel.
        async fn fetch(&mut self, command: &str) -> (u16, String, Vec<u8>) {
            let port = self.pasv().await;
            self.stream
                .write_all(format!("{command}\r\n").as_bytes())
                .await
                .unwrap();
            self.stream.flush().await.unwrap();
            let mut data = self.data_connect(port).await;
            assert_eq!(self.read_reply().await.0, 150);
            let mut body = Vec::new();
            data.read_to_end(&mut body).await.unwrap();
            let (code, line) = self.read_reply().await;
            (code, line, body)
        }
    }

    #[tokio::test]
    async fn a_sliced_file_uploaded_to_the_relay_lands_on_the_printer() {
        // The whole reason this exists: Bambu Studio's "send print" uploads
        // first, and its upload goes to whichever host it was pointed at.
        let files = FakeFiles::new();
        let addr = start(FtpRelay::new(
            CODE,
            Arc::clone(&files) as Arc<dyn PrinterFiles>,
        ))
        .await;
        let mut client = TestClient::connect(addr).await;
        client.login().await;
        assert_eq!(client.cmd("TYPE I").await.0, 200);
        assert_eq!(client.cmd("CWD /cache").await.0, 250);

        let body = b"PK\x03\x04 pretend this is a sliced plate".to_vec();
        let (code, line) = client.upload("plate.gcode.3mf", &body).await;
        assert_eq!(code, 226, "{line}");

        let uploads = files.uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(
            uploads[0].0, "/cache/plate.gcode.3mf",
            "relative to the CWD"
        );
        assert_eq!(uploads[0].1, body, "byte-for-byte");
    }

    #[tokio::test]
    async fn a_printer_that_refuses_the_upload_is_reported_not_papered_over() {
        // The client's upload to US succeeded; the printer is the half that
        // said no, and a 226 here would have Studio start a print of a file
        // that isn't there.
        let files = FakeFiles::refusing();
        let addr = start(FtpRelay::new(CODE, files as Arc<dyn PrinterFiles>)).await;
        let mut client = TestClient::connect(addr).await;
        client.login().await;
        let (code, line) = client.upload("/cache/plate.gcode.3mf", b"body").await;
        assert_eq!(code, 550, "{line}");
        assert!(
            line.contains("SD card"),
            "the printer's reason survives: {line}"
        );
    }

    #[tokio::test]
    async fn a_download_streams_the_printers_copy_back() {
        let files = FakeFiles::with_file("/model/a.gcode.3mf", b"the real bytes");
        let addr = start(FtpRelay::new(CODE, files as Arc<dyn PrinterFiles>)).await;
        let mut client = TestClient::connect(addr).await;
        client.login().await;
        let (code, line, body) = client.fetch("RETR /model/a.gcode.3mf").await;
        assert_eq!(code, 226, "{line}");
        assert_eq!(body, b"the real bytes");
    }

    #[tokio::test]
    async fn a_missing_file_is_a_550_with_no_empty_transfer() {
        let addr = start(FtpRelay::new(
            CODE,
            FakeFiles::new() as Arc<dyn PrinterFiles>,
        ))
        .await;
        let mut client = TestClient::connect(addr).await;
        client.login().await;
        // No PASV: the fetch is refused before a data channel is ever needed,
        // which is the point — an empty "successful" download would have the
        // client believe it had the file.
        let (code, line) = client.cmd("RETR /model/nope.3mf").await;
        assert_eq!(code, 550, "{line}");
    }

    #[tokio::test]
    async fn a_listing_relays_the_printers_own_lines() {
        let addr = start(FtpRelay::new(
            CODE,
            FakeFiles::new() as Arc<dyn PrinterFiles>,
        ))
        .await;
        let mut client = TestClient::connect(addr).await;
        client.login().await;
        let (code, line, body) = client.fetch("LIST /cache").await;
        assert_eq!(code, 226, "{line}");
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("a.gcode.3mf"), "{text}");
        assert!(
            text.contains("Jun 13 18:11"),
            "verbatim, not re-rendered: {text}"
        );
        assert!(text.ends_with("\r\n"), "lines are CRLF-terminated");
    }

    #[tokio::test]
    async fn size_comes_from_the_printers_listing() {
        let addr = start(FtpRelay::new(
            CODE,
            FakeFiles::new() as Arc<dyn PrinterFiles>,
        ))
        .await;
        let mut client = TestClient::connect(addr).await;
        client.login().await;
        assert_eq!(
            client.cmd("SIZE /cache/a.gcode.3mf").await,
            (213, "213 92763".to_string())
        );
        assert_eq!(client.cmd("SIZE /cache/nope.3mf").await.0, 550);
    }

    #[tokio::test]
    async fn a_wrong_access_code_gets_nowhere() {
        let files = FakeFiles::new();
        let addr = start(FtpRelay::new(
            CODE,
            Arc::clone(&files) as Arc<dyn PrinterFiles>,
        ))
        .await;
        let mut client = TestClient::connect(addr).await;
        assert_eq!(client.cmd("USER bblp").await.0, 331);
        assert_eq!(client.cmd("PASS 00000000").await.0, 530);
        assert_eq!(client.cmd("PASV").await.0, 530);
        assert!(files.uploads().is_empty());
    }

    #[tokio::test]
    async fn a_read_only_relay_serves_reads_and_refuses_writes() {
        let files = FakeFiles::with_file("/model/a.gcode.3mf", b"bytes");
        let addr = start(FtpRelay::read_only(
            CODE,
            Arc::clone(&files) as Arc<dyn PrinterFiles>,
        ))
        .await;
        let mut client = TestClient::connect(addr).await;
        client.login().await;
        // Reads still work.
        let (code, _, body) = client.fetch("RETR /model/a.gcode.3mf").await;
        assert_eq!(code, 226);
        assert_eq!(body, b"bytes");
        // Writes do not.
        assert_eq!(client.cmd("STOR /cache/x.3mf").await.0, 550);
        assert_eq!(client.cmd("DELE /model/a.gcode.3mf").await.0, 550);
        assert!(files.uploads().is_empty());
    }

    #[tokio::test]
    async fn a_stranger_racing_the_data_port_is_dropped_and_the_real_transfer_still_works() {
        // The PASV port is open to the network for as long as it exists, and
        // whoever connects first gets it. Unchecked, another host can race the
        // real client: winning a STOR lets it choose the bytes this relay
        // uploads to a machine that heats and moves — no access code needed,
        // just presence and timing.
        //
        // Refusing the stranger is only half of it. If losing the race also
        // failed the client's transfer, the same race would be an
        // unauthenticated denial of service: win it repeatedly and every STOR
        // and RETR answers 425. So the stranger is dropped and the listener
        // keeps waiting for the host that owns the control connection.
        //
        // Loopback gives two distinct source addresses (127.0.0.1 and
        // 127.0.0.2) on one machine, which is exactly the check.
        let files = FakeFiles::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = crate::tls::emulated_printer_server_config(SERIAL).unwrap();
        tokio::spawn(
            FtpRelay::new(CODE, Arc::clone(&files) as Arc<dyn PrinterFiles>).serve(listener, tls),
        );

        let mut client = TestClient::connect(addr).await;
        client.login().await;
        let port = client.pasv().await;
        client
            .stream
            .write_all(b"STOR /cache/plate.3mf\r\n")
            .await
            .unwrap();
        client.stream.flush().await.unwrap();
        assert_eq!(client.read_reply().await.0, 150);

        // The stranger gets there first, from a different source address, and
        // writes what it would like the printer to build.
        let hijack = tokio::net::TcpSocket::new_v4().unwrap();
        hijack.bind("127.0.0.2:0".parse().unwrap()).unwrap();
        if let Ok(mut taken) = hijack
            .connect(format!("127.0.0.1:{port}").parse().unwrap())
            .await
        {
            let _ = taken.write_all(b"attacker bytes").await;
            let _ = taken.shutdown().await;
        }

        // The real client then connects and its transfer completes normally.
        let mut data = client.data_connect(port).await;
        data.write_all(b"the real plate").await.unwrap();
        data.shutdown().await.unwrap();
        let mut rest = Vec::new();
        let _ = data.read_to_end(&mut rest).await;
        drop(data);

        let (code, line) = client.read_reply().await;
        assert_eq!(
            code, 226,
            "the stranger must not break the transfer: {line}"
        );
        let uploads = files.uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(
            uploads[0].1, b"the real plate",
            "the printer must receive the client's bytes, never the stranger's"
        );
    }

    #[tokio::test]
    async fn an_upload_can_be_renamed_into_place() {
        // Upload-to-temp-then-rename is a common client idiom and the printer
        // supports it (docs/protocol.md), so refusing it here would break a
        // flow the printer alone would have satisfied.
        let files = FakeFiles::new();
        let addr = start(FtpRelay::new(
            CODE,
            Arc::clone(&files) as Arc<dyn PrinterFiles>,
        ))
        .await;
        let mut client = TestClient::connect(addr).await;
        client.login().await;
        client.cmd("CWD /cache").await;
        assert_eq!(client.upload("upload.tmp", b"plate bytes").await.0, 226);
        assert_eq!(client.cmd("RNFR upload.tmp").await.0, 350);
        assert_eq!(client.cmd("RNTO plate.gcode.3mf").await.0, 250);
        assert_eq!(
            files.renames(),
            vec![(
                "/cache/upload.tmp".to_string(),
                "/cache/plate.gcode.3mf".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn directories_are_relayed_and_read_only_refuses_them() {
        let files = FakeFiles::new();
        let addr = start(FtpRelay::new(
            CODE,
            Arc::clone(&files) as Arc<dyn PrinterFiles>,
        ))
        .await;
        let mut client = TestClient::connect(addr).await;
        client.login().await;
        assert_eq!(client.cmd("MKD /cache/sub").await.0, 257);
        assert_eq!(client.cmd("RMD /cache/sub").await.0, 250);
        assert_eq!(files.dirs(), vec!["mkdir /cache/sub", "rmdir /cache/sub"]);

        let ro = FakeFiles::new();
        let ro_addr = start(FtpRelay::read_only(
            CODE,
            Arc::clone(&ro) as Arc<dyn PrinterFiles>,
        ))
        .await;
        let mut ro_client = TestClient::connect(ro_addr).await;
        ro_client.login().await;
        assert_eq!(ro_client.cmd("MKD /cache/sub").await.0, 550);
        assert_eq!(ro_client.cmd("RNFR /a").await.0, 350);
        assert_eq!(ro_client.cmd("RNTO /b").await.0, 550);
        assert!(ro.dirs().is_empty() && ro.renames().is_empty());
    }

    #[tokio::test]
    async fn a_refused_transfer_leaves_exactly_one_reply_on_the_control_stream() {
        // FTP is strictly one reply per command. This used to write 425 from the
        // data-channel setup and then 550 from the caller, so the *next* command
        // read the stale one and every reply after it was off by one — invisible
        // to a test that only checks the first reply, which is what the earlier
        // version of this test did.
        // A file that exists, so RETR reaches the missing-PASV check rather
        // than being refused earlier for not being there.
        let files = FakeFiles::with_file("/model/a.3mf", b"bytes");
        let addr = start(FtpRelay::new(CODE, files as Arc<dyn PrinterFiles>)).await;
        let mut client = TestClient::connect(addr).await;
        client.login().await;
        assert_eq!(client.cmd("STOR /cache/x.3mf").await.0, 425);
        // If a second reply were queued, this would read it instead of 215.
        assert_eq!(
            client.cmd("SYST").await.0,
            215,
            "the session should still be in step"
        );
        assert_eq!(client.cmd("RETR /model/a.3mf").await.0, 425);
        assert_eq!(client.cmd("SYST").await.0, 215);
        assert_eq!(client.cmd("LIST").await.0, 425);
        assert_eq!(client.cmd("PWD").await.0, 257);
    }

    #[tokio::test]
    async fn several_commands_arriving_in_one_packet_are_all_answered() {
        // FTP clients pipeline, and a TCP read can carry several lines.
        let addr = start(FtpRelay::new(
            CODE,
            FakeFiles::new() as Arc<dyn PrinterFiles>,
        ))
        .await;
        let mut client = TestClient::connect(addr).await;
        client
            .stream
            .write_all(b"USER bblp\r\nPASS 12345678\r\nSYST\r\n")
            .await
            .unwrap();
        client.stream.flush().await.unwrap();
        assert_eq!(client.read_reply().await.0, 331);
        assert_eq!(client.read_reply().await.0, 230);
        assert_eq!(client.read_reply().await.0, 215);
    }
}
