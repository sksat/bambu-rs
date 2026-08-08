//! MQTT 3.1.1 wire codec — the **server** side of the conversation.
//!
//! Pure and I/O-free: bytes in, [`Packet`] out, and back. This exists because
//! emulating a printer means *being* an MQTT broker, and `rumqttc` (the client we
//! use to talk to a real printer) only speaks the client half. Only the subset a
//! Bambu client exercises is implemented — CONNECT/CONNACK, PUBLISH/PUBACK,
//! SUBSCRIBE/SUBACK, UNSUBSCRIBE/UNSUBACK, PINGREQ/PINGRESP, DISCONNECT — and
//! QoS 2 is rejected rather than half-supported (the printer protocol never uses
//! it; see [`MqttError::Unsupported`]).

use std::fmt;

/// The report topic for a serial: `device/{serial}/report` (printer → client).
pub fn report_topic(serial: &str) -> String {
    format!("device/{serial}/report")
}

/// The request topic for a serial: `device/{serial}/request` (client → printer).
pub fn request_topic(serial: &str) -> String {
    format!("device/{serial}/request")
}

/// A decoded MQTT 3.1.1 control packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    Connect(Connect),
    ConnAck {
        session_present: bool,
        code: ConnectCode,
    },
    Publish(Publish),
    PubAck {
        packet_id: u16,
    },
    Subscribe {
        packet_id: u16,
        filters: Vec<(String, u8)>,
    },
    SubAck {
        packet_id: u16,
        codes: Vec<u8>,
    },
    Unsubscribe {
        packet_id: u16,
        filters: Vec<String>,
    },
    UnsubAck {
        packet_id: u16,
    },
    PingReq,
    PingResp,
    Disconnect,
}

/// A CONNECT packet's payload — everything the emulator needs to authenticate.
///
/// The password is bytes on the wire and a secret here: it is redacted from
/// `Debug` so an access code never reaches a log line.
#[derive(Clone, PartialEq, Eq)]
pub struct Connect {
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub keep_alive: u16,
    pub clean_session: bool,
}

impl fmt::Debug for Connect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connect")
            .field("client_id", &self.client_id)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("keep_alive", &self.keep_alive)
            .field("clean_session", &self.clean_session)
            .finish()
    }
}

/// A PUBLISH packet. `packet_id` is present exactly when `qos > 0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publish {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
    pub packet_id: Option<u16>,
    pub retain: bool,
    pub dup: bool,
}

impl Publish {
    /// A fire-and-forget (QoS 0) publish — what the report topic uses.
    pub fn at_most_once(topic: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            topic: topic.into(),
            payload: payload.into(),
            qos: 0,
            packet_id: None,
            retain: false,
            dup: false,
        }
    }
}

/// CONNACK return codes (MQTT 3.1.1 §3.2.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectCode {
    Accepted,
    UnacceptableProtocolVersion,
    IdentifierRejected,
    ServerUnavailable,
    BadCredentials,
    NotAuthorized,
}

impl ConnectCode {
    pub fn as_byte(self) -> u8 {
        match self {
            ConnectCode::Accepted => 0,
            ConnectCode::UnacceptableProtocolVersion => 1,
            ConnectCode::IdentifierRejected => 2,
            ConnectCode::ServerUnavailable => 3,
            ConnectCode::BadCredentials => 4,
            ConnectCode::NotAuthorized => 5,
        }
    }
}

/// A malformed or unsupported packet. Any of these is fatal for the connection:
/// MQTT has no error packet, so the only correct response is to close.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MqttError {
    #[error("malformed packet: {0}")]
    Malformed(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("remaining length exceeds the 268435455-byte maximum")]
    LengthOverflow,
}

/// Decode one packet from the front of `buf`.
///
/// Returns `Ok(None)` when `buf` holds only part of a packet — the caller reads
/// more and tries again — and `Ok(Some((packet, consumed)))` otherwise, where
/// `consumed` is how many bytes to drop from the front of the buffer.
pub fn decode(buf: &[u8]) -> Result<Option<(Packet, usize)>, MqttError> {
    let Some((remaining_len, header_len)) = decode_remaining_length(buf)? else {
        return Ok(None);
    };
    let total = header_len + remaining_len;
    if buf.len() < total {
        return Ok(None);
    }
    let first = buf[0];
    let body = &buf[header_len..total];
    let packet = decode_body(first, body)?;
    Ok(Some((packet, total)))
}

/// Split the variable-length "remaining length" field, returning
/// `(remaining_length, bytes_of_fixed_header)`. `Ok(None)` = need more bytes.
fn decode_remaining_length(buf: &[u8]) -> Result<Option<(usize, usize)>, MqttError> {
    if buf.is_empty() {
        return Ok(None);
    }
    let mut value = 0usize;
    let mut multiplier = 1usize;
    for i in 0..4 {
        let Some(&byte) = buf.get(1 + i) else {
            return Ok(None); // length field itself is incomplete
        };
        value += (byte & 0x7F) as usize * multiplier;
        if byte & 0x80 == 0 {
            return Ok(Some((value, 2 + i)));
        }
        multiplier *= 128;
    }
    Err(MqttError::LengthOverflow)
}

fn decode_body(first: u8, body: &[u8]) -> Result<Packet, MqttError> {
    let packet_type = first >> 4;
    let flags = first & 0x0F;
    let mut r = Reader::new(body);
    match packet_type {
        1 => decode_connect(&mut r),
        3 => decode_publish(flags, &mut r),
        4 => Ok(Packet::PubAck {
            packet_id: r.u16()?,
        }),
        8 => decode_subscribe(&mut r),
        10 => decode_unsubscribe(&mut r),
        12 => Ok(Packet::PingReq),
        14 => Ok(Packet::Disconnect),
        // A broker never *receives* these; a client sending one is broken.
        2 | 5 | 6 | 7 | 9 | 11 | 13 => Err(MqttError::Malformed("server-to-client packet type")),
        _ => Err(MqttError::Malformed("unknown packet type")),
    }
}

fn decode_connect(r: &mut Reader<'_>) -> Result<Packet, MqttError> {
    let protocol = r.string()?;
    let level = r.u8()?;
    // 3.1.1 ("MQTT"/4) is what every Bambu client speaks. 3.1 ("MQIsdp"/3) is
    // accepted too: same packet layout for the subset we implement.
    if !(protocol == "MQTT" && level == 4 || protocol == "MQIsdp" && level == 3) {
        return Err(MqttError::Unsupported("MQTT protocol version"));
    }
    let flags = r.u8()?;
    let keep_alive = r.u16()?;
    let client_id = r.string()?;
    let has_will = flags & 0x04 != 0;
    if has_will {
        // Parsed and dropped: the emulator has no session state worth willing
        // away, but the fields must be consumed to reach username/password.
        let _will_topic = r.string()?;
        let _will_payload = r.bytes()?;
    }
    let username = if flags & 0x80 != 0 {
        Some(r.string()?)
    } else {
        None
    };
    let password = if flags & 0x40 != 0 {
        Some(String::from_utf8_lossy(r.bytes()?).into_owned())
    } else {
        None
    };
    Ok(Packet::Connect(Connect {
        client_id,
        username,
        password,
        keep_alive,
        clean_session: flags & 0x02 != 0,
    }))
}

fn decode_publish(flags: u8, r: &mut Reader<'_>) -> Result<Packet, MqttError> {
    let qos = (flags & 0x06) >> 1;
    if qos > 1 {
        // QoS 2 needs the PUBREC/PUBREL/PUBCOMP dance; the printer protocol
        // never uses it, so refuse rather than pretend.
        return Err(MqttError::Unsupported("QoS 2"));
    }
    let topic = r.string()?;
    let packet_id = if qos > 0 { Some(r.u16()?) } else { None };
    Ok(Packet::Publish(Publish {
        topic,
        payload: r.rest().to_vec(),
        qos,
        packet_id,
        retain: flags & 0x01 != 0,
        dup: flags & 0x08 != 0,
    }))
}

fn decode_subscribe(r: &mut Reader<'_>) -> Result<Packet, MqttError> {
    let packet_id = r.u16()?;
    let mut filters = Vec::new();
    while !r.is_empty() {
        let filter = r.string()?;
        let qos = r.u8()? & 0x03;
        filters.push((filter, qos));
    }
    if filters.is_empty() {
        return Err(MqttError::Malformed("SUBSCRIBE with no filters"));
    }
    Ok(Packet::Subscribe { packet_id, filters })
}

fn decode_unsubscribe(r: &mut Reader<'_>) -> Result<Packet, MqttError> {
    let packet_id = r.u16()?;
    let mut filters = Vec::new();
    while !r.is_empty() {
        filters.push(r.string()?);
    }
    if filters.is_empty() {
        return Err(MqttError::Malformed("UNSUBSCRIBE with no filters"));
    }
    Ok(Packet::Unsubscribe { packet_id, filters })
}

/// Serialize a packet to its wire bytes.
pub fn encode(packet: &Packet) -> Vec<u8> {
    let mut out = Vec::new();
    match packet {
        Packet::ConnAck {
            session_present,
            code,
        } => {
            out.push(0x20);
            out.push(2);
            out.push(u8::from(*session_present));
            out.push(code.as_byte());
        }
        Packet::Publish(p) => {
            let mut body = Vec::new();
            put_string(&mut body, &p.topic);
            if p.qos > 0 {
                body.extend_from_slice(&p.packet_id.unwrap_or(0).to_be_bytes());
            }
            body.extend_from_slice(&p.payload);
            let flags = (u8::from(p.dup) << 3) | ((p.qos & 0x03) << 1) | u8::from(p.retain);
            out.push(0x30 | flags);
            put_remaining_length(&mut out, body.len());
            out.extend_from_slice(&body);
        }
        Packet::PubAck { packet_id } => {
            out.push(0x40);
            out.push(2);
            out.extend_from_slice(&packet_id.to_be_bytes());
        }
        Packet::SubAck { packet_id, codes } => {
            let mut body = Vec::new();
            body.extend_from_slice(&packet_id.to_be_bytes());
            body.extend_from_slice(codes);
            out.push(0x90);
            put_remaining_length(&mut out, body.len());
            out.extend_from_slice(&body);
        }
        Packet::UnsubAck { packet_id } => {
            out.push(0xB0);
            out.push(2);
            out.extend_from_slice(&packet_id.to_be_bytes());
        }
        Packet::PingResp => {
            out.push(0xD0);
            out.push(0);
        }
        Packet::PingReq => {
            out.push(0xC0);
            out.push(0);
        }
        Packet::Disconnect => {
            out.push(0xE0);
            out.push(0);
        }
        // Client-to-server packets: encoded only so tests can drive a decoder
        // with real bytes (the emulator never sends one).
        Packet::Connect(c) => {
            let mut body = Vec::new();
            put_string(&mut body, "MQTT");
            body.push(4);
            let flags = (u8::from(c.username.is_some()) << 7)
                | (u8::from(c.password.is_some()) << 6)
                | (u8::from(c.clean_session) << 1);
            body.push(flags);
            body.extend_from_slice(&c.keep_alive.to_be_bytes());
            put_string(&mut body, &c.client_id);
            if let Some(u) = &c.username {
                put_string(&mut body, u);
            }
            if let Some(p) = &c.password {
                put_string(&mut body, p);
            }
            out.push(0x10);
            put_remaining_length(&mut out, body.len());
            out.extend_from_slice(&body);
        }
        Packet::Subscribe { packet_id, filters } => {
            let mut body = Vec::new();
            body.extend_from_slice(&packet_id.to_be_bytes());
            for (f, qos) in filters {
                put_string(&mut body, f);
                body.push(*qos);
            }
            out.push(0x82); // SUBSCRIBE's reserved flags are 0b0010
            put_remaining_length(&mut out, body.len());
            out.extend_from_slice(&body);
        }
        Packet::Unsubscribe { packet_id, filters } => {
            let mut body = Vec::new();
            body.extend_from_slice(&packet_id.to_be_bytes());
            for f in filters {
                put_string(&mut body, f);
            }
            out.push(0xA2);
            put_remaining_length(&mut out, body.len());
            out.extend_from_slice(&body);
        }
    }
    out
}

fn put_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn put_remaining_length(out: &mut Vec<u8>, mut len: usize) {
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if len == 0 {
            break;
        }
    }
}

/// A cursor over a packet body that never panics: every read is bounds-checked
/// into [`MqttError::Malformed`].
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn u8(&mut self) -> Result<u8, MqttError> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or(MqttError::Malformed("truncated byte"))?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16, MqttError> {
        let hi = self.u8()? as u16;
        let lo = self.u8()? as u16;
        Ok((hi << 8) | lo)
    }

    fn bytes(&mut self) -> Result<&'a [u8], MqttError> {
        let len = self.u16()? as usize;
        let end = self
            .pos
            .checked_add(len)
            .filter(|e| *e <= self.buf.len())
            .ok_or(MqttError::Malformed("truncated length-prefixed field"))?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn string(&mut self) -> Result<String, MqttError> {
        let b = self.bytes()?;
        String::from_utf8(b.to_vec()).map_err(|_| MqttError::Malformed("non-UTF-8 string"))
    }

    fn rest(&mut self) -> &'a [u8] {
        let out = &self.buf[self.pos.min(self.buf.len())..];
        self.pos = self.buf.len();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connect(username: &str, password: &str) -> Packet {
        Packet::Connect(Connect {
            client_id: "bambu-studio-1".to_string(),
            username: Some(username.to_string()),
            password: Some(password.to_string()),
            keep_alive: 60,
            clean_session: true,
        })
    }

    #[test]
    fn a_connect_round_trips_with_its_credentials() {
        let want = connect("bblp", "12345678");
        let bytes = encode(&want);
        let (got, used) = decode(&bytes).unwrap().expect("a whole packet");
        assert_eq!(got, want);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn a_partial_packet_asks_for_more_bytes_rather_than_erroring() {
        // The emulator reads off a socket, so every prefix of a packet must be
        // "not yet", never a decode error — otherwise a split TCP segment kills
        // an otherwise-healthy connection.
        let bytes = encode(&connect("bblp", "12345678"));
        for cut in 0..bytes.len() {
            assert_eq!(
                decode(&bytes[..cut]),
                Ok(None),
                "prefix of {cut} bytes should be incomplete"
            );
        }
        assert!(decode(&bytes).unwrap().is_some());
    }

    #[test]
    fn decode_reports_how_much_it_consumed_so_a_pipelined_stream_keeps_its_place() {
        // Bambu Studio pipelines SUBSCRIBE right behind CONNECT; both can land in
        // one read.
        let mut stream = encode(&connect("bblp", "12345678"));
        stream.extend_from_slice(&encode(&Packet::Subscribe {
            packet_id: 1,
            filters: vec![("device/0309FA/report".to_string(), 0)],
        }));
        let (first, used) = decode(&stream).unwrap().unwrap();
        assert!(matches!(first, Packet::Connect(_)));
        let (second, _) = decode(&stream[used..]).unwrap().unwrap();
        assert_eq!(
            second,
            Packet::Subscribe {
                packet_id: 1,
                filters: vec![("device/0309FA/report".to_string(), 0)],
            }
        );
    }

    #[test]
    fn the_password_is_redacted_from_debug() {
        // It is the LAN access code — the one secret this whole crate hides.
        let dbg = format!("{:?}", connect("bblp", "12345678"));
        assert!(dbg.contains("<redacted>"), "{dbg}");
        assert!(!dbg.contains("12345678"), "{dbg}");
    }

    #[test]
    fn a_qos1_publish_carries_its_packet_id_and_a_qos0_one_does_not() {
        let q1 = Packet::Publish(Publish {
            topic: "device/0309FA/request".to_string(),
            payload: br#"{"pushing":{"command":"pushall"}}"#.to_vec(),
            qos: 1,
            packet_id: Some(7),
            retain: false,
            dup: false,
        });
        let (got, _) = decode(&encode(&q1)).unwrap().unwrap();
        assert_eq!(got, q1);

        let q0 = Packet::Publish(Publish::at_most_once(
            "device/0309FA/report",
            b"{}".to_vec(),
        ));
        let (got, _) = decode(&encode(&q0)).unwrap().unwrap();
        assert_eq!(got, q0);
    }

    #[test]
    fn a_payload_over_127_bytes_uses_a_multi_byte_length() {
        // A pushall snapshot is kilobytes; a single-byte length field would
        // silently truncate it.
        let payload = vec![b'x'; 5000];
        let p = Packet::Publish(Publish::at_most_once(
            "device/0309FA/report",
            payload.clone(),
        ));
        let bytes = encode(&p);
        let (got, used) = decode(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        match got {
            Packet::Publish(pp) => assert_eq!(pp.payload, payload),
            other => panic!("expected a publish, got {other:?}"),
        }
    }

    #[test]
    fn ping_and_disconnect_are_two_byte_packets() {
        assert_eq!(encode(&Packet::PingResp), vec![0xD0, 0x00]);
        let (got, used) = decode(&[0xC0, 0x00]).unwrap().unwrap();
        assert_eq!((got, used), (Packet::PingReq, 2));
        let (got, _) = decode(&[0xE0, 0x00]).unwrap().unwrap();
        assert_eq!(got, Packet::Disconnect);
    }

    #[test]
    fn subscribe_and_unsubscribe_round_trip_their_filters() {
        let sub = Packet::Subscribe {
            packet_id: 3,
            filters: vec![
                ("device/0309FA/report".to_string(), 0),
                ("device/0309FA/status".to_string(), 1),
            ],
        };
        assert_eq!(decode(&encode(&sub)).unwrap().unwrap().0, sub);
        let unsub = Packet::Unsubscribe {
            packet_id: 4,
            filters: vec!["device/0309FA/report".to_string()],
        };
        assert_eq!(decode(&encode(&unsub)).unwrap().unwrap().0, unsub);
    }

    #[test]
    fn a_connect_with_a_will_still_finds_the_credentials_after_it() {
        // Home Assistant's MQTT client sets a will. The will fields sit between
        // the client id and the username, so skipping them wrong would read the
        // password out of the will payload.
        let mut body = Vec::new();
        put_string(&mut body, "MQTT");
        body.push(4);
        body.push(0x80 | 0x40 | 0x04 | 0x02); // username+password+will+clean
        body.extend_from_slice(&60u16.to_be_bytes());
        put_string(&mut body, "ha-client");
        put_string(&mut body, "will/topic");
        put_string(&mut body, "offline");
        put_string(&mut body, "bblp");
        put_string(&mut body, "12345678");
        let mut bytes = vec![0x10];
        put_remaining_length(&mut bytes, body.len());
        bytes.extend_from_slice(&body);

        let (got, _) = decode(&bytes).unwrap().unwrap();
        match got {
            Packet::Connect(c) => {
                assert_eq!(c.client_id, "ha-client");
                assert_eq!(c.username.as_deref(), Some("bblp"));
                assert_eq!(c.password.as_deref(), Some("12345678"));
            }
            other => panic!("expected a connect, got {other:?}"),
        }
    }

    #[test]
    fn qos2_is_refused_rather_than_silently_downgraded() {
        // Half-implementing QoS 2 would hang the client waiting for a PUBREC.
        let bytes = [0x34, 0x08, 0x00, 0x03, b'a', b'/', b'b', 0x00, 0x01, 0x00];
        assert_eq!(decode(&bytes), Err(MqttError::Unsupported("QoS 2")));
    }

    #[test]
    fn a_truncated_string_inside_a_complete_packet_is_malformed_not_incomplete() {
        // The fixed header claims 4 bytes and we have them, but the topic's
        // length prefix points past the end: that is a broken client, and
        // returning "need more bytes" would wait forever.
        let bytes = [0x30, 0x04, 0x00, 0x40, b'a', b'b'];
        assert!(matches!(decode(&bytes), Err(MqttError::Malformed(_))));
    }

    #[test]
    fn an_over_long_remaining_length_is_rejected() {
        let bytes = [0x30, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F];
        assert_eq!(decode(&bytes), Err(MqttError::LengthOverflow));
    }

    #[test]
    fn an_old_protocol_name_is_accepted_but_an_unknown_level_is_not() {
        let mut body = Vec::new();
        put_string(&mut body, "MQTT");
        body.push(5); // MQTT 5.0 — not implemented here
        body.push(0x02);
        body.extend_from_slice(&60u16.to_be_bytes());
        put_string(&mut body, "c");
        let mut bytes = vec![0x10];
        put_remaining_length(&mut bytes, body.len());
        bytes.extend_from_slice(&body);
        assert_eq!(
            decode(&bytes),
            Err(MqttError::Unsupported("MQTT protocol version"))
        );
    }
}
