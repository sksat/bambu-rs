//! The device-detect frame — what Bambu Studio asks before it will talk to you.
//!
//! Studio's "add printer by IP" dialog does **not** open MQTT first. It opens a
//! short TCP connection to port 3000 (plain) or 3002 (TLS), exchanges one framed
//! JSON message, and closes. If that fails it abandons the whole attempt: MQTT is
//! never tried, the access code is never even sent, and the failure is invisible
//! from the server side because a refused SYN appears in no log and no socket
//! list. A relay that serves 8883 flawlessly is still unreachable by IP without
//! this.
//!
//! Captured from the real A1 mini with `tcpdump` while Studio added it — so the
//! shape below is observation of our own hardware, not someone else's source:
//!
//! ```text
//! → a5 a5 3a 00 {"login":{"command":"detect","sequence_id":"20004"}} a7 a7
//! ← a5 a5 b5 00 {"login":{"command":"detect","sequence_id":"20004",
//!                "id":"0309FA432200488","model":"N1","name":"3DP-030-488",
//!                "version":"01.07.02.00","bind":"free","connect":"lan",
//!                "dev_cap":1}} a7 a7
//! ```
//!
//! `bind: "free"` and `connect: "lan"` are not decoration: Studio refuses a
//! device that reports `cloud`, and refuses one already `occupied`.

use serde_json::Value;

/// Frame magic, first two bytes.
pub const MAGIC: [u8; 2] = [0xA5, 0xA5];
/// Frame trailer, last two bytes.
pub const TRAILER: [u8; 2] = [0xA7, 0xA7];
/// Magic + length + trailer. The length field counts these too.
pub const OVERHEAD: usize = 6;

/// Refuse anything larger rather than buffer it.
///
/// A real detect frame is a couple of hundred bytes. The length field is a u16,
/// so 65535 is the structural ceiling anyway — this is the *useful* bound, the
/// one that stops a peer making us wait on 64 KB that is never coming.
pub const MAX_FRAME: usize = 8 * 1024;

/// A malformed detect frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DetectError {
    #[error("frame does not start with the a5a5 magic")]
    BadMagic,
    #[error("frame does not end with the a7a7 trailer")]
    BadTrailer,
    #[error("frame claims {0} bytes, which is shorter than the framing itself")]
    TooShort(usize),
    #[error("frame claims {0} bytes, over the {MAX_FRAME}-byte limit")]
    TooLong(usize),
    #[error("payload is not JSON: {0}")]
    NotJson(String),
}

/// Wrap `payload` in the frame the printer speaks.
///
/// The length field is the **whole** frame, framing included — 58 for the
/// 52-byte request above, not 52.
pub fn encode(payload: &Value) -> Vec<u8> {
    let body = payload.to_string().into_bytes();
    let total = body.len() + OVERHEAD;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&(total as u16).to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&TRAILER);
    out
}

/// Decode one frame from the front of `buf`.
///
/// `Ok(None)` means "not all here yet" — the bytes arrive off a socket, so every
/// prefix has to be patience rather than an error.
pub fn decode(buf: &[u8]) -> Result<Option<(Value, usize)>, DetectError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    if buf[0..2] != MAGIC {
        return Err(DetectError::BadMagic);
    }
    let total = u16::from_le_bytes([buf[2], buf[3]]) as usize;
    if total < OVERHEAD {
        return Err(DetectError::TooShort(total));
    }
    if total > MAX_FRAME {
        return Err(DetectError::TooLong(total));
    }
    if buf.len() < total {
        return Ok(None);
    }
    if buf[total - 2..total] != TRAILER {
        return Err(DetectError::BadTrailer);
    }
    let body = &buf[4..total - 2];
    let value = serde_json::from_slice(body).map_err(|e| DetectError::NotJson(e.to_string()))?;
    Ok(Some((value, total)))
}

/// Whether this is the `login`/`detect` enquiry, and its sequence id if so.
pub fn detect_sequence_id(request: &Value) -> Option<&str> {
    let login = request.get("login")?;
    (login.get("command").and_then(Value::as_str) == Some("detect"))
        .then(|| login.get("sequence_id").and_then(Value::as_str))
        .flatten()
}

/// Build the reply, in the shape the printer sends.
///
/// `bind` and `connect` are fixed rather than parameters: a relay that reported
/// anything but `free`/`lan` would be refused by Studio, and there is no state
/// here that could honestly say otherwise.
pub fn detect_reply(
    sequence_id: &str,
    serial: &str,
    model: &str,
    name: &str,
    version: &str,
) -> Value {
    serde_json::json!({"login": {
        "command": "detect",
        "sequence_id": sequence_id,
        "id": serial,
        "model": model,
        "name": name,
        "version": version,
        "bind": "free",
        "connect": "lan",
        "dev_cap": 1,
    }})
}

/// Adjust a reply relayed from the real printer to describe the relay.
///
/// Identity — `id`, `model`, `name`, `version` — is passed through untouched:
/// those are facts about the machine, and the relay has no business inventing
/// them. `bind` and `connect` are overwritten, because they describe
/// *availability*, and the relay's availability is not the printer's.
///
/// This matters more than it looks. The relay holds an MQTT connection of its
/// own, which is the whole point of it; if the printer answers `occupied` while
/// that connection is up, passing it through would make Studio refuse the relay
/// precisely when the relay is working. The relay is free — it multiplexes.
pub fn as_relay_reply(mut printer_reply: Value) -> Value {
    if let Some(login) = printer_reply
        .get_mut("login")
        .and_then(Value::as_object_mut)
    {
        login.insert("bind".into(), Value::from("free"));
        login.insert("connect".into(), Value::from("lan"));
    }
    printer_reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The exact bytes captured from the A1 mini, request and reply.
    const REQUEST: &[u8] =
        b"\xa5\xa5\x3a\x00{\"login\":{\"command\":\"detect\",\"sequence_id\":\"20004\"}}\xa7\xa7";

    #[test]
    fn the_captured_request_decodes() {
        let (value, used) = decode(REQUEST).unwrap().expect("a whole frame");
        assert_eq!(used, REQUEST.len());
        assert_eq!(used, 58, "the capture was 58 bytes on the wire");
        assert_eq!(detect_sequence_id(&value), Some("20004"));
    }

    #[test]
    fn the_length_field_counts_the_framing_too() {
        // 0x3a is 58 — the whole frame, not the 52 bytes of JSON inside it.
        // Getting this backwards produces a frame the printer silently ignores.
        assert_eq!(
            u16::from_le_bytes([REQUEST[2], REQUEST[3]]) as usize,
            REQUEST.len()
        );
        let round = encode(&json!({"login": {"command": "detect", "sequence_id": "20004"}}));
        assert_eq!(
            round, REQUEST,
            "our encoder must reproduce the capture byte for byte"
        );
    }

    #[test]
    fn a_reply_carries_what_studio_checks() {
        let reply = detect_reply(
            "20004",
            "0309FA432200488",
            "N1",
            "3DP-030-488",
            "01.07.02.00",
        );
        let (back, _) = decode(&encode(&reply)).unwrap().unwrap();
        assert_eq!(back["login"]["sequence_id"], "20004", "the id is echoed");
        assert_eq!(back["login"]["id"], "0309FA432200488");
        assert_eq!(back["login"]["model"], "N1");
        // Studio refuses a device reporting cloud, or one already bound.
        assert_eq!(back["login"]["connect"], "lan");
        assert_eq!(back["login"]["bind"], "free");
    }

    #[test]
    fn every_prefix_of_a_frame_is_patience_not_an_error() {
        // The bytes come off a socket; a split segment must not kill the probe.
        for cut in 0..REQUEST.len() {
            assert_eq!(decode(&REQUEST[..cut]), Ok(None), "prefix of {cut}");
        }
        assert!(decode(REQUEST).unwrap().is_some());
    }

    #[test]
    fn a_frame_that_is_not_one_is_refused() {
        assert_eq!(decode(b"\x00\x00\x06\x00xx"), Err(DetectError::BadMagic));
        // Right magic and length, wrong trailer.
        assert_eq!(
            decode(b"\xa5\xa5\x08\x00{}\x00\x00"),
            Err(DetectError::BadTrailer)
        );
        // A length that cannot even hold its own framing.
        assert_eq!(
            decode(b"\xa5\xa5\x02\x00\xa7\xa7"),
            Err(DetectError::TooShort(2))
        );
        // Well-framed, but the payload is not JSON.
        assert!(matches!(
            decode(b"\xa5\xa5\x09\x00not\xa7\xa7"),
            Err(DetectError::NotJson(_))
        ));
    }

    #[test]
    fn an_absurd_length_is_refused_rather_than_waited_on() {
        // A u16 length caps at 65535 by construction, so the bound that earns
        // its keep is the smaller one: without it a peer could announce 64 KB
        // and leave us holding the connection for bytes that never arrive.
        let mut frame = vec![0xA5, 0xA5];
        frame.extend_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(decode(&frame), Err(DetectError::TooLong(65535)));
        // Just inside the bound is patience, not an error.
        let mut ok = vec![0xA5, 0xA5];
        ok.extend_from_slice(&(MAX_FRAME as u16).to_le_bytes());
        assert_eq!(decode(&ok), Ok(None));
    }

    #[test]
    fn a_relayed_reply_keeps_the_printers_identity_but_not_its_availability() {
        // What the printer says while the relay itself is connected to it.
        let from_printer = json!({"login": {
            "command": "detect", "sequence_id": "20004", "id": "0309FA432200488",
            "model": "N1", "name": "3DP-030-488", "version": "01.07.02.00",
            "bind": "occupied", "connect": "lan", "dev_cap": 1,
        }});
        let relayed = as_relay_reply(from_printer);
        // Identity is the machine's own; the relay must not invent it.
        assert_eq!(relayed["login"]["id"], "0309FA432200488");
        assert_eq!(relayed["login"]["version"], "01.07.02.00");
        assert_eq!(relayed["login"]["name"], "3DP-030-488");
        // Availability is the relay's, and the relay multiplexes. Passing
        // `occupied` through would make Studio refuse us exactly when the
        // relay's own connection is up — that is, whenever it is working.
        assert_eq!(relayed["login"]["bind"], "free");
        assert_eq!(relayed["login"]["connect"], "lan");
    }

    #[test]
    fn a_relayed_reply_that_is_shaped_wrong_is_passed_through_unharmed() {
        // Not our place to fabricate a login object that wasn't there.
        let odd = json!({"unexpected": 1});
        assert_eq!(as_relay_reply(odd.clone()), odd);
    }

    #[test]
    fn something_that_is_not_a_detect_is_not_mistaken_for_one() {
        assert_eq!(
            detect_sequence_id(&json!({"login": {"command": "bind"}})),
            None
        );
        assert_eq!(
            detect_sequence_id(&json!({"print": {"command": "detect"}})),
            None
        );
    }
}
