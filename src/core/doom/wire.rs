//! What the engine writes back, and how it is told apart from a picture.
//!
//! The engine is a separate process ([`crate::server::doom`] spawns it) and
//! everything it says arrives on one pipe: pictures in the printer's own
//! camera framing, and status records carrying [`Vitals`]. This is the framing
//! that keeps the two from ever being mistaken for each other.

use crate::core::camerad::{CameraError, FRAME_HEADER, frame_len};

use super::vitals::Vitals;

// ---- what the engine writes back ----------------------------------------

/// The word that marks a status record, in the slot a frame leaves at zero.
///
/// Readable in a hexdump, which is the only debugger this pipe has.
pub const STATUS_MAGIC: [u8; 4] = *b"DOOM";

/// The largest status payload that will be read rather than refused. Four bytes
/// are used today; the rest is room to add a field without a flag day.
pub const MAX_STATUS: usize = 64;

/// What the engine just put on its stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    /// A picture: `len` bytes of JPEG follow.
    Frame { len: usize },
    /// The player's state: `len` bytes of [`Vitals`] follow.
    Status { len: usize },
}

/// A record header that made no sense.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("{0}")]
    Frame(#[from] CameraError),
    #[error("a status record claims {0} bytes, more than the {MAX_STATUS} one can be")]
    StatusSize(u32),
}

/// The 16-byte header in front of a status payload of `len` bytes.
///
/// The length word is **zero**, which is the point: no frame may be smaller
/// than [`crate::core::camerad::MIN_FRAME`], so a reader that knows only about
/// frames refuses this outright instead of handing a client four bytes of
/// binary as a photograph. The engine writes the same shape — see
/// `tools/doom/doomgeneric_bambu.c`.
pub fn status_header(len: u32) -> [u8; FRAME_HEADER] {
    let mut header = [0u8; FRAME_HEADER];
    // header[0..4] stays zero: not a frame length, and cannot become one.
    header[4..8].copy_from_slice(&STATUS_MAGIC);
    header[8..12].copy_from_slice(&len.to_le_bytes());
    header
}

/// Read one record header: a picture, or something the game wants to say.
pub fn classify_record(header: &[u8; FRAME_HEADER]) -> Result<Record, RecordError> {
    if header[0..4] == [0, 0, 0, 0] && header[4..8] == STATUS_MAGIC {
        let len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        if len as usize > MAX_STATUS {
            return Err(RecordError::StatusSize(len));
        }
        return Ok(Record::Status { len: len as usize });
    }
    Ok(Record::Frame {
        len: frame_len(header)?,
    })
}

/// Read a status payload.
///
/// A short payload is not an error, it is an engine that says less than this
/// one asks for; a long one is an engine that says more, and the extra is
/// ignored rather than refused. Either way what is missing stays `None`, which
/// leaves the printer's own reading alone.
///
/// A negative number means "no player" — the title screen, the intermission —
/// and is the reason [`Vitals`] holds options rather than numbers.
pub fn parse_vitals(payload: &[u8]) -> Vitals {
    let field = |at: usize| -> Option<i16> {
        let bytes = payload.get(at..at + 2)?;
        let value = i16::from_le_bytes([bytes[0], bytes[1]]);
        (value >= 0).then_some(value)
    };
    Vitals {
        health: field(0),
        armour: field(2),
    }
}

/// The payload the engine sends. Here rather than only in C so the two sides
/// are one definition, and so a test can write what the engine writes.
pub fn vitals_payload(vitals: Vitals) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    for value in [vitals.health, vitals.armour] {
        out.extend_from_slice(&value.unwrap_or(-1).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_record_can_never_be_read_as_a_picture() {
        // The two records share one pipe. The length word of a status record is
        // zero, which is below the smallest frame anything here will accept, so
        // a reader that knows only about frames refuses it loudly instead of
        // handing four bytes of binary to a JPEG decoder.
        let header = status_header(4);
        assert_eq!(
            frame_len(&header),
            Err(CameraError::FrameSize(0)),
            "a frame reader must refuse this, not decode it"
        );
        assert_eq!(classify_record(&header).unwrap(), Record::Status { len: 4 });
    }

    #[test]
    fn a_frame_header_is_still_a_frame() {
        let header = crate::core::camerad::frame_header(4096);
        assert_eq!(
            classify_record(&header).unwrap(),
            Record::Frame { len: 4096 }
        );
        // …and a frame whose size our own client would refuse is refused here.
        assert!(matches!(
            classify_record(&crate::core::camerad::frame_header(10)),
            Err(RecordError::Frame(CameraError::FrameSize(10)))
        ));
    }

    #[test]
    fn a_status_record_that_claims_the_world_is_refused() {
        // The reader allocates what the header asks for.
        let mut header = status_header(0);
        header[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            classify_record(&header),
            Err(RecordError::StatusSize(u32::MAX))
        );
    }

    #[test]
    fn what_the_engine_writes_is_what_the_relay_reads() {
        let vitals = Vitals {
            health: Some(66),
            armour: Some(12),
        };
        assert_eq!(parse_vitals(&vitals_payload(vitals)), vitals);
        // "no player" survives the round trip as unknown rather than as zero,
        // which is a dead one.
        assert_eq!(
            parse_vitals(&vitals_payload(Vitals::default())),
            Vitals::default()
        );
    }

    #[test]
    fn an_engine_that_says_more_or_less_than_this_one_asks_for_still_works() {
        // Room to add a field later without a flag day in either direction.
        let mut longer = vitals_payload(Vitals {
            health: Some(10),
            armour: Some(20),
        });
        longer.extend_from_slice(&[9, 9, 9, 9]);
        assert_eq!(
            parse_vitals(&longer),
            Vitals {
                health: Some(10),
                armour: Some(20)
            }
        );
        // Half a record: what is there is read, what is not stays unknown.
        assert_eq!(
            parse_vitals(&[100, 0]),
            Vitals {
                health: Some(100),
                armour: None
            }
        );
        assert_eq!(parse_vitals(&[]), Vitals::default());
    }
}
