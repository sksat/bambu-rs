//! The chamber-camera wire format — both halves of it.
//!
//! A client opens TLS to TCP 6000, sends an 80-byte authentication packet, and
//! is then sent JPEG frames, each behind a 16-byte header whose first `u32` is
//! the frame's length. [`crate::camera`] has spoken the client half of this
//! against a real printer since before any of it lived here; the pieces moved
//! here so the emulated printer can answer with the same shapes rather than a
//! second, drifting copy.
//!
//! **What is known, and what is not.** The auth packet and the length-prefixed
//! header are how our client talks to a printer. Everything else is thinner:
//!
//! - 12 of the 16 header bytes have never been seen. Our client reads 16 and
//!   uses the first 4; see [`UNKNOWN_HEADER_TAIL`].
//! - The client takes **one frame per connection** and hangs up, so continuous
//!   streaming — whether the printer paces frames, expects keepalives, or ever
//!   re-authenticates — is not something this crate has observed.
//! - The one A1 mini here has a hardware-dead camera (front-cover FPC), so the
//!   real server side cannot be watched on this bench at all.
//!
//! That ignorance is why the emulated side is built from what a *client* needs
//! rather than from a capture, and why the pieces are shared: the production
//! client is then a witness the server has to satisfy.

use serde_json::Value;

/// Where a real printer serves the camera stream.
pub const CAMERA_PORT: u16 = 6000;

/// Bytes in the frame header, of which only the first four are understood.
pub const FRAME_HEADER: usize = 16;

/// Bytes in the authentication packet the client sends first.
pub const AUTH_PACKET: usize = 80;

/// The username every LAN service on the printer uses.
pub const USER: &str = "bblp";

/// The 12 header bytes after the length, which nobody here has ever seen.
///
/// **A guess, and deliberately the emptiest one.** Our client reads the header
/// and uses only the length; the community clients whose source can be read do
/// the same. Any invented value would be exactly as likely to be wrong as zeros
/// while looking like knowledge. A test pins this so that changing it has to be
/// a decision, and `docs/protocol.md` carries it as an open unknown.
pub const UNKNOWN_HEADER_TAIL: [u8; 12] = [0; 12];

/// Frames outside this range are refused rather than forwarded.
///
/// The lower bound is what [`crate::camera`] has always enforced against a real
/// printer, and it doubles as a filter for a substituted source handing us an
/// HTML error page where a photograph should be.
pub const MIN_FRAME: usize = 1000;
/// Upper bound, likewise from the client.
pub const MAX_FRAME: usize = 8_000_000;

/// A camera exchange that did not make sense.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CameraError {
    #[error("authentication packet is {0} bytes, expected {AUTH_PACKET}")]
    AuthSize(usize),
    #[error("authentication packet does not begin with the expected header")]
    AuthHeader,
    #[error("authentication is for user {0:?}, not {USER:?}")]
    AuthUser(String),
    #[error("frame claims {0} bytes, outside {MIN_FRAME}..={MAX_FRAME}")]
    FrameSize(u32),
    #[error("payload is not a JPEG")]
    NotJpeg,
}

/// The 80-byte authentication packet: header, then user and access code, each
/// null-padded into 32 bytes.
pub fn auth_packet(access_code: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(AUTH_PACKET);
    p.extend_from_slice(&0x40u32.to_le_bytes());
    p.extend_from_slice(&0x3000u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&padded32(USER));
    p.extend_from_slice(&padded32(access_code));
    p
}

fn padded32(s: &str) -> [u8; 32] {
    let mut field = [0u8; 32];
    let bytes = s.as_bytes();
    let n = bytes.len().min(32);
    field[..n].copy_from_slice(&bytes[..n]);
    field
}

fn unpad(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Who a client says it is.
///
/// The code is carried so the caller can compare it, and redacted from `Debug`
/// the way [`crate::config::ResolvedTarget`] does it — a failing assertion or a
/// stray `{:?}` in a log is exactly where a secret gets away.
#[derive(PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub access_code: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .field("access_code", &"<redacted>")
            .finish()
    }
}

/// Read the authentication packet from the front of `buf`.
///
/// `Ok(None)` means "not all 80 bytes yet" — they arrive off a socket, so every
/// prefix has to be patience rather than a rejection.
pub fn parse_auth(buf: &[u8]) -> Result<Option<Credentials>, CameraError> {
    if buf.len() < AUTH_PACKET {
        return Ok(None);
    }
    if buf[0..4] != 0x40u32.to_le_bytes() || buf[4..8] != 0x3000u32.to_le_bytes() {
        return Err(CameraError::AuthHeader);
    }
    let user = unpad(&buf[16..48]);
    if user != USER {
        return Err(CameraError::AuthUser(user));
    }
    Ok(Some(Credentials {
        user,
        access_code: unpad(&buf[48..80]),
    }))
}

/// The 16-byte header that precedes a frame of `len` bytes.
pub fn frame_header(len: u32) -> [u8; FRAME_HEADER] {
    let mut header = [0u8; FRAME_HEADER];
    header[0..4].copy_from_slice(&len.to_le_bytes());
    header[4..].copy_from_slice(&UNKNOWN_HEADER_TAIL);
    header
}

/// The frame length a header announces, refusing sizes our own client would.
pub fn frame_len(header: &[u8; FRAME_HEADER]) -> Result<usize, CameraError> {
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    if (MIN_FRAME as u32..=MAX_FRAME as u32).contains(&len) {
        Ok(len as usize)
    } else {
        Err(CameraError::FrameSize(len))
    }
}

/// Whether `bytes` is plausibly a JPEG: the start and end markers, and a size
/// the far side will accept.
///
/// A substituted camera is an ordinary HTTP server, and an ordinary HTTP server
/// answers with an error page when it is unhappy. Framing that as a photograph
/// would put a wall of HTML into a client's decoder.
pub fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= MIN_FRAME
        && bytes.len() <= MAX_FRAME
        && bytes.starts_with(&[0xFF, 0xD8])
        && bytes.ends_with(&[0xFF, 0xD9])
}

/// Tell a client the printer has a camera, in a report about to be relayed.
///
/// Serving the camera port is not enough on its own. A client reads
/// `print.ipcam.ipcam_dev` and, finding `"0"`, never opens a liveview at all —
/// observed with Bambu Studio against this relay, which connected over MQTT and
/// then made no attempt on the camera port. The A1 mini here reports `"0"`
/// honestly, because its built-in camera is physically dead.
///
/// **This is the one place the relay contradicts the printer about the printer.**
/// Elsewhere it only ever corrects claims about *itself* — the detect reply says
/// `bind: free` because the relay is free, whatever the machine is doing. Here
/// it says a camera exists where the machine says none does. What makes it
/// defensible is that, from the client's side, it is true: there really is a
/// camera on that port, and pictures really do come out of it. What makes it
/// safe is that it only happens when a camera was named for substitution — an
/// unsubstituted relay repeats the printer's `"0"` and no client goes looking.
///
/// Returns whether anything changed, so a caller can avoid re-encoding a
/// message it did not touch.
pub fn claim_camera(message: &mut Value) -> bool {
    let Some(print) = message.get_mut("print").and_then(Value::as_object_mut) else {
        return false;
    };
    // Only in a report. An ACK wears the same envelope, and bolting device
    // state onto a command's answer would put it somewhere no client looks.
    if !print.contains_key("ipcam") {
        return false;
    }
    let Some(ipcam) = print.get_mut("ipcam").and_then(Value::as_object_mut) else {
        return false;
    };
    if ipcam.get("ipcam_dev").and_then(Value::as_str) == Some(PRESENT) {
        return false;
    }
    ipcam.insert("ipcam_dev".to_string(), Value::from(PRESENT));
    true
}

/// What `ipcam_dev` says when a camera is there. A string, as the printer sends
/// it — a client comparing against `"1"` would not recognise the number 1.
const PRESENT: &str = "1";

/// Splits an MJPEG `multipart/x-mixed-replace` body into single frames.
///
/// A substituted camera speaks HTTP, and the endless-stream form of that is a
/// multipart body: a boundary line, a few headers, one JPEG, repeat. Nothing in
/// this crate took that apart before — the dashboard reverse-proxies the body
/// untouched and the timelapse recorder writes it to a file whole — but a
/// printer's camera port carries *frames*, so somewhere they have to be cut
/// apart.
///
/// Incremental by construction, like [`parse_auth`]: the bytes come off a
/// socket, so "not yet" is the common answer rather than a failure.
pub struct Multipart {
    /// The delimiters this stream might use, longest first.
    ///
    /// Normally one: the parameter with `--` in front, as the standard says.
    /// A camera that writes `boundary=--frame` gets two, because in practice
    /// such a camera delimits with `--frame` and not the `----frame` the
    /// standard would produce — and guessing wrong means never finding a frame
    /// at all, which looks exactly like a camera that is switched off.
    markers: Vec<Vec<u8>>,
}

impl Multipart {
    /// Read the boundary out of a `Content-Type`, if it says one.
    ///
    /// Servers quote it or don't, and some put the `--` in the parameter as
    /// well as on the wire; both are accepted rather than guessed at.
    pub fn from_content_type(content_type: &str) -> Option<Self> {
        let (_, rest) = content_type.split_once("boundary=")?;
        let value = rest
            .split(';')
            .next()?
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        if value.is_empty() {
            return None;
        }
        let spec = [b"--".as_slice(), value.as_bytes()].concat();
        let mut markers = vec![spec];
        if let Some(bare) = value.strip_prefix("--") {
            markers.push([b"--".as_slice(), bare.as_bytes()].concat());
        }
        // Longest first, so a buffer containing `----frame` is not matched as
        // the shorter `--frame` two characters early.
        markers.sort_by_key(|m| std::cmp::Reverse(m.len()));
        Some(Self { markers })
    }

    /// The next frame in `buf`, and how much of `buf` it used up.
    ///
    /// `Ok(None)` means the frame has not all arrived. The payload is returned
    /// as a range rather than a copy so the caller can decide whether it is
    /// worth keeping — most frames are, one in a while is an error page.
    pub fn next_frame(
        &self,
        buf: &[u8],
    ) -> Result<Option<(std::ops::Range<usize>, usize)>, CameraError> {
        let Some((start, marker)) = self.find_marker(buf) else {
            return Ok(None);
        };
        let after_marker = start + marker.len();
        // Headers end at a blank line; until then this part is incomplete.
        let Some(gap) = find(&buf[after_marker..], b"\r\n\r\n") else {
            return Ok(None);
        };
        let headers = &buf[after_marker..after_marker + gap];
        let body = after_marker + gap + 4;

        // A declared length is the reliable route; scanning for the next
        // boundary is the fallback for servers that omit it.
        if let Some(len) = content_length(headers) {
            if buf.len() < body + len {
                return Ok(None);
            }
            return Ok(Some((body..body + len, body + len)));
        }
        let Some((next, _)) = self.find_marker(&buf[body..]) else {
            return Ok(None);
        };
        let mut end = body + next;
        // The CRLF before a boundary belongs to the delimiter, not the picture.
        if buf[body..end].ends_with(b"\r\n") {
            end -= 2;
        }
        Ok(Some((body..end, end)))
    }
}

impl Multipart {
    /// The earliest delimiter in `buf`, and which one it was.
    fn find_marker<'a>(&'a self, buf: &[u8]) -> Option<(usize, &'a [u8])> {
        self.markers
            .iter()
            .filter_map(|m| find(buf, m).map(|at| (at, m.as_slice())))
            .min_by_key(|(at, m)| (*at, std::cmp::Reverse(m.len())))
    }
}

/// The `Content-Length` of a part, if it declares one.
fn content_length(headers: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(headers);
    for line in text.split("\r\n") {
        // Skip lines that are not `name: value` rather than giving up on the
        // whole header block — the slice starts with the CRLF that ended the
        // boundary line, so the first thing here is always blank.
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_auth_packet_keeps_the_layout_the_printer_was_given() {
        // These offsets are what `camera.rs` has sent to a real printer; this
        // module took the code over, so it inherits the obligation.
        let p = auth_packet("12345678");
        assert_eq!(p.len(), AUTH_PACKET);
        assert_eq!(&p[0..4], &0x40u32.to_le_bytes());
        assert_eq!(&p[4..8], &0x3000u32.to_le_bytes());
        assert_eq!(&p[16..20], b"bblp");
        assert_eq!(p[20], 0, "the user field is null-padded");
        assert_eq!(&p[48..56], b"12345678");
    }

    #[test]
    fn what_the_client_sends_is_what_the_server_reads() {
        let creds = parse_auth(&auth_packet("87654321"))
            .unwrap()
            .expect("a whole packet");
        assert_eq!(creds.user, USER);
        assert_eq!(creds.access_code, "87654321");
    }

    #[test]
    fn every_prefix_of_the_auth_packet_is_patience_not_an_error() {
        let full = auth_packet("12345678");
        for cut in 0..AUTH_PACKET {
            assert!(
                matches!(parse_auth(&full[..cut]), Ok(None)),
                "prefix of {cut} should be incomplete, not decided"
            );
        }
        assert!(parse_auth(&full).unwrap().is_some());
    }

    #[test]
    fn a_packet_that_is_not_one_is_refused() {
        let mut wrong_magic = auth_packet("12345678");
        wrong_magic[0] ^= 0xFF;
        assert_eq!(parse_auth(&wrong_magic), Err(CameraError::AuthHeader));

        let mut wrong_user = auth_packet("12345678");
        wrong_user[16..20].copy_from_slice(b"root");
        assert_eq!(
            parse_auth(&wrong_user),
            Err(CameraError::AuthUser("root".into()))
        );
    }

    #[test]
    fn a_full_length_access_code_is_not_truncated_or_overrun() {
        // 32 bytes exactly fills the field, leaving no terminator to find.
        let code = "C".repeat(32);
        let creds = parse_auth(&auth_packet(&code)).unwrap().unwrap();
        assert_eq!(creds.access_code, code);
    }

    #[test]
    fn the_frame_header_says_the_length_and_admits_the_rest_is_unknown() {
        let header = frame_header(4096);
        assert_eq!(header.len(), FRAME_HEADER);
        assert_eq!(
            &header[0..4],
            &4096u32.to_le_bytes(),
            "little-endian length"
        );
        // Pinned so that inventing a value for the twelve bytes we have never
        // seen has to be a deliberate change, not a drive-by.
        assert_eq!(&header[4..], &[0u8; 12], "the unknown tail stays empty");
        assert_eq!(frame_len(&header).unwrap(), 4096);
    }

    #[test]
    fn a_frame_size_our_own_client_would_refuse_is_refused_here_too() {
        // camera.rs has always rejected these against a real printer; the
        // emulated side must not emit what the real side would never send.
        assert_eq!(
            frame_len(&frame_header(999)),
            Err(CameraError::FrameSize(999))
        );
        assert_eq!(
            frame_len(&frame_header(8_000_001)),
            Err(CameraError::FrameSize(8_000_001))
        );
        assert!(frame_len(&frame_header(MIN_FRAME as u32)).is_ok());
        assert!(frame_len(&frame_header(MAX_FRAME as u32)).is_ok());
    }

    /// A JPEG-shaped blob big enough to pass the size gate.
    fn jpeg(fill: u8) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.extend(std::iter::repeat_n(fill, MIN_FRAME));
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    fn multipart_body(boundary: &str, frames: &[Vec<u8>], with_length: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for f in frames {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            out.extend_from_slice(b"Content-Type: image/jpeg\r\n");
            if with_length {
                out.extend_from_slice(format!("Content-Length: {}\r\n", f.len()).as_bytes());
            }
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(f);
            out.extend_from_slice(b"\r\n");
        }
        out
    }

    fn drain(mp: &Multipart, body: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut at = 0;
        while let Ok(Some((range, used))) = mp.next_frame(&body[at..]) {
            out.push(body[at + range.start..at + range.end].to_vec());
            at += used;
        }
        out
    }

    #[test]
    fn a_relayed_report_can_say_the_camera_is_there() {
        // The shape the A1 mini really sends, dead camera and all.
        let mut report = serde_json::json!({"print": {
            "command": "push_status", "msg": 0,
            "ipcam": {
                "ipcam_dev": "0", "ipcam_record": "enable", "mode_bits": 3,
                "resolution": "1080p", "timelapse": "disable", "tutk_server": "disable",
            },
        }});
        assert!(claim_camera(&mut report));
        assert_eq!(report["print"]["ipcam"]["ipcam_dev"], "1");
        // A string, not the number: a client comparing against "1" would not
        // recognise 1.
        assert!(report["print"]["ipcam"]["ipcam_dev"].is_string());
        // Everything else is the printer's to say, and is left alone.
        assert_eq!(report["print"]["ipcam"]["resolution"], "1080p");
        assert_eq!(report["print"]["ipcam"]["tutk_server"], "disable");
        assert_eq!(report["print"]["msg"], 0);
        // Idempotent: a second pass has nothing to change.
        assert!(!claim_camera(&mut report));
    }

    #[test]
    fn nothing_that_is_not_a_camera_report_is_touched() {
        // An ACK wears the same envelope as a report. Adding device state to a
        // command's answer would put it where no client reads it, and would
        // make the ACK a thing it is not.
        let mut ack = serde_json::json!({"print": {"sequence_id": "1", "result": "success"}});
        let before = ack.clone();
        assert!(!claim_camera(&mut ack));
        assert_eq!(ack, before);

        // A report from a model whose camera the relay is not standing in for
        // keeps whatever it said.
        let mut other = serde_json::json!({"info": {"command": "get_version"}});
        assert!(!claim_camera(&mut other));
    }

    #[test]
    fn the_boundary_is_read_however_the_server_spells_it() {
        for header in [
            "multipart/x-mixed-replace; boundary=frame",
            "multipart/x-mixed-replace;boundary=\"frame\"",
            "multipart/x-mixed-replace; boundary=frame; charset=binary",
        ] {
            let mp = Multipart::from_content_type(header).expect(header);
            assert_eq!(mp.markers, vec![b"--frame".to_vec()], "{header}");
        }
        assert!(Multipart::from_content_type("image/jpeg").is_none());
        assert!(Multipart::from_content_type("multipart/x-mixed-replace; boundary=").is_none());
    }

    #[test]
    fn a_boundary_that_already_has_dashes_is_still_found() {
        // Cameras that write `boundary=--frame` delimit with `--frame`, not the
        // `----frame` the standard implies. Following the standard alone here
        // finds no frames at all, which is indistinguishable from a camera that
        // is switched off — so both spellings are tried.
        let want = vec![jpeg(0x88), jpeg(0x99)];
        let mp = Multipart::from_content_type("multipart/x-mixed-replace; boundary=--frame")
            .expect("a boundary");
        assert_eq!(drain(&mp, &multipart_body("frame", &want, true)), want);
        // And the by-the-book spelling still works.
        assert_eq!(drain(&mp, &multipart_body("--frame", &want, true)), want);
    }

    #[test]
    fn frames_come_out_whole_with_a_declared_length() {
        let want = vec![jpeg(0x11), jpeg(0x22), jpeg(0x33)];
        let body = multipart_body("frame", &want, true);
        let mp = Multipart::from_content_type("multipart/x-mixed-replace; boundary=frame").unwrap();
        let got = drain(&mp, &body);
        assert_eq!(got, want);
        assert!(got.iter().all(|f| is_jpeg(f)));
    }

    #[test]
    fn frames_come_out_whole_without_one() {
        // Plenty of MJPEG servers omit Content-Length and rely on the next
        // boundary; the CRLF before it belongs to the delimiter, not the image,
        // and keeping it would corrupt every frame by two bytes.
        let want = vec![jpeg(0x44), jpeg(0x55)];
        let body = multipart_body("frame", &want, false);
        let mp = Multipart::from_content_type("multipart/x-mixed-replace; boundary=frame").unwrap();
        // The final frame has no boundary after it yet, so it is still pending.
        let got = drain(&mp, &body);
        assert_eq!(got, want[..1].to_vec());
        assert!(is_jpeg(&got[0]));
    }

    #[test]
    fn a_frame_split_across_reads_is_waited_for_not_mangled() {
        let want = jpeg(0x66);
        let body = multipart_body("frame", std::slice::from_ref(&want), true);
        let mp = Multipart::from_content_type("multipart/x-mixed-replace; boundary=frame").unwrap();
        for cut in 0..body.len() {
            match mp.next_frame(&body[..cut]) {
                Ok(None) => {}
                Ok(Some((range, _))) => {
                    // Whenever it does decide, it must be the whole frame.
                    assert_eq!(
                        &body[range],
                        &want[..],
                        "wrong frame from a {cut}-byte prefix"
                    );
                }
                Err(e) => panic!("prefix of {cut} should not be an error: {e}"),
            }
        }
        let (range, used) = mp.next_frame(&body).unwrap().unwrap();
        assert_eq!(&body[range], &want[..]);
        assert!(used <= body.len());
    }

    #[test]
    fn leading_rubbish_before_the_first_boundary_is_skipped() {
        // Some servers send a preamble; it is not part of any frame.
        let want = jpeg(0x77);
        let mut body = b"ignore me\r\n".to_vec();
        body.extend(multipart_body("frame", std::slice::from_ref(&want), true));
        let mp = Multipart::from_content_type("multipart/x-mixed-replace; boundary=frame").unwrap();
        let (range, _) = mp.next_frame(&body).unwrap().unwrap();
        assert_eq!(&body[range], &want[..]);
    }

    #[test]
    fn an_error_page_is_not_mistaken_for_a_photograph() {
        // The substituted camera is an ordinary HTTP server, and one of those
        // answers with HTML when it is unhappy.
        let html = b"<html><body>502 Bad Gateway</body></html>".repeat(40);
        assert!(!is_jpeg(&html));

        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend(std::iter::repeat_n(0x00, MIN_FRAME));
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        assert!(is_jpeg(&jpeg));

        // Right markers, too small to be a picture.
        assert!(!is_jpeg(&[0xFF, 0xD8, 0xFF, 0xD9]));
        // Starts right, never ends — a truncated read.
        let truncated = [vec![0xFF, 0xD8], vec![0x11; MIN_FRAME]].concat();
        assert!(!is_jpeg(&truncated));
    }
}
