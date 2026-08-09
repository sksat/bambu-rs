//! The printer's control surface, read as a game controller.
//!
//! `bambu serve --emulate` already sees every command a LAN client sends before
//! it is forwarded ([`crate::core::emulate`]). This turns those commands into
//! DOOM input instead: a jog on Y is a step forward, the home button is the
//! trigger, the chamber light opens doors. The picture goes back the other way,
//! through the camera port ([`crate::core::camerad`]), so a client that thinks
//! it is watching a print is watching E1M1.
//!
//! Pure, like the rest of `core`: a request payload in, key presses out. The
//! clock belongs to the caller — every entry point takes a monotonic
//! millisecond stamp — so the hold logic is driven from a test without waiting
//! for anything.
//!
//! **A press has a duration**, which is the one thing that makes this more than
//! a lookup table. DOOM moves you for as long as a key is down, so a button
//! that is only ever tapped would leave the player twitching on the spot. The
//! duration comes from the jog itself: the millimetres the client asked the
//! machine to move become the milliseconds the key is held, so Studio's 1 mm
//! and 10 mm buttons are a nudge and a step.
//!
//! **The game comes back the other way too**, and not only as pictures: the
//! player's health is reported as the nozzle temperature, so a client that
//! knows nothing about any of this still shows the game state on the readout it
//! puts in the largest type on the screen. See [`Vitals`].

use std::collections::BTreeMap;

use serde_json::Value;

use crate::core::camerad::{CameraError, FRAME_HEADER, frame_len};
use crate::core::report::is_status_report;
use crate::core::safety::{MAX_JOG_MM, TempLimits};

/// A DOOM keyboard code, as `doomkeys.h` defines them.
///
/// The engine on the other end of the pipe is a DOOM port, so these are its
/// codes and not some intermediate vocabulary of ours — an extra translation
/// layer would only be somewhere else for the mapping to be wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Forward,
    Back,
    TurnLeft,
    TurnRight,
    StrafeLeft,
    StrafeRight,
    Fire,
    Use,
    Escape,
    Pause,
    /// Weapon slot 1–4. Out-of-range slots never reach here: [`holds_for`] only
    /// builds one from the printer's four speed levels.
    Weapon(u8),
}

impl Key {
    /// The byte the engine expects, from `doomkeys.h`.
    pub fn code(self) -> u8 {
        match self {
            Key::Forward => 0xad,     // KEY_UPARROW
            Key::Back => 0xaf,        // KEY_DOWNARROW
            Key::TurnLeft => 0xac,    // KEY_LEFTARROW
            Key::TurnRight => 0xae,   // KEY_RIGHTARROW
            Key::StrafeLeft => 0xa0,  // KEY_STRAFE_L
            Key::StrafeRight => 0xa1, // KEY_STRAFE_R
            Key::Fire => 0xa3,        // KEY_FIRE
            Key::Use => 0xa2,         // KEY_USE
            Key::Escape => 27,        // KEY_ESCAPE
            Key::Pause => 0xff,       // KEY_PAUSE
            // DOOM reads weapon selection as the plain ASCII digit.
            Key::Weapon(slot) => b'0' + slot,
        }
    }

    /// A short name for a log line. The relay's own log is the only window on
    /// whether a button did anything, so it says which key it became.
    pub fn name(self) -> String {
        match self {
            Key::Forward => "forward".into(),
            Key::Back => "back".into(),
            Key::TurnLeft => "turn left".into(),
            Key::TurnRight => "turn right".into(),
            Key::StrafeLeft => "strafe left".into(),
            Key::StrafeRight => "strafe right".into(),
            Key::Fire => "fire".into(),
            Key::Use => "use".into(),
            Key::Escape => "escape".into(),
            Key::Pause => "pause".into(),
            Key::Weapon(slot) => format!("weapon {slot}"),
        }
    }
}

/// One key, held for a while.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold {
    pub key: Key,
    pub ms: u64,
}

/// A press or a release, on its way to the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub key: Key,
    pub pressed: bool,
}

impl Event {
    /// The two bytes the engine reads: whether it is a press, and which key.
    ///
    /// This is `DG_GetKey`'s own shape — `int* pressed, unsigned char* key` —
    /// flattened onto a pipe, so the engine's side of it is a `read` and a
    /// switch rather than a parser.
    pub fn to_wire(self) -> [u8; 2] {
        [u8::from(self.pressed), self.key.code()]
    }
}

/// Milliseconds a key is held per millimetre of commanded jog.
///
/// Not a physical constant and nothing depends on it being right: it is the
/// gearing between a printer's idea of a small movement and a game's. At 25,
/// Studio's 10 mm button is a quarter-second stride and its 1 mm button is a
/// single step — which is roughly what those buttons feel like on the machine.
const MS_PER_MM: u64 = 25;

/// DOOM's tic, in milliseconds — it runs at 35 of them a second.
const TIC_MS: u64 = 1000 / 35;

/// The shortest useful press.
///
/// DOOM samples the keyboard once per tic, so a key pressed and released inside
/// one tic never appears down and the button does nothing at all. Two tics is
/// the smallest hold that is certain to be seen; every press gets at least this
/// long, including the ones with no distance behind them (fire, use, weapons).
const MIN_HOLD_MS: u64 = 2 * TIC_MS;

/// Which axis a move word names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    X,
    Y,
    Z,
    E,
}

/// What this client request means to the game.
///
/// An empty result means "nothing" — the command is still consumed (see
/// [`crate::core::emulate::ControlPolicy::Intercept`]), it just does not press
/// anything. That is the common case for the parts of the protocol with no
/// button behind them, and it is deliberately not an error: a client is free to
/// send whatever it likes, and a printer that is secretly a game should be no
/// harder to talk to than one that isn't.
pub fn holds_for(payload: &Value) -> Vec<Hold> {
    let command = |category: &str| {
        payload
            .pointer(&format!("/{category}/command"))
            .and_then(Value::as_str)
    };
    let param = |category: &str| {
        payload
            .pointer(&format!("/{category}/param"))
            .and_then(Value::as_str)
    };
    match (command("print"), command("system")) {
        (Some("gcode_line"), _) => param("print").map(gcode_holds).unwrap_or_default(),
        // The four speed levels are the four weapon slots — fist, pistol,
        // shotgun, chaingun. The printer numbers them 1–4 and so does DOOM.
        (Some("print_speed"), _) => param("print")
            .and_then(|p| p.trim().parse::<u8>().ok())
            .filter(|slot| (1..=4).contains(slot))
            .map(|slot| vec![tap(Key::Weapon(slot))])
            .unwrap_or_default(),
        // Pausing a print pauses the game; the printer has no "unpause", and
        // neither does DOOM — the same key does both.
        (Some("pause" | "resume"), _) => vec![tap(Key::Pause)],
        // Stopping a print is the closest thing a printer has to "I want out
        // of this", which is what the menu key means.
        (Some("stop"), _) => vec![tap(Key::Escape)],
        // Either edge of the light toggle. A switch that has to be flipped back
        // before it can be used again would be half a button.
        (_, Some("ledctrl")) => vec![tap(Key::Use)],
        _ => Vec::new(),
    }
}

/// A press with no distance behind it: held just long enough to register.
fn tap(key: Key) -> Hold {
    Hold {
        key,
        ms: MIN_HOLD_MS,
    }
}

/// The keys a `gcode_line` payload presses.
///
/// A payload may be several lines — this crate's own jog sends
/// `"G91\nG1 Z5 F600\nG90"` as one command, and the A1 runs all of it — so the
/// modal state (`G90`/`G91`, `M82`/`M83`) is tracked across the payload rather
/// than assumed per line.
fn gcode_holds(param: &str) -> Vec<Hold> {
    // Relative until told otherwise. A jog is always relative, and a client
    // that sends a bare `G1 Y10` with no mode word means "move ten".
    let mut axes_relative = true;
    let mut e_relative = true;
    let mut holds = Vec::new();
    for line in param.split(['\n', '\r']) {
        let body = line.split(';').next().unwrap_or("").to_ascii_uppercase();
        let words = words(&body);
        // A leading `N5` is a line number, not the command — the same tolerance
        // `crate::core::safety` extends, and for the same reason.
        let command = match words.first() {
            Some(('N', _)) => words.get(1),
            other => other,
        };
        let Some((letter, number)) = command.copied() else {
            continue;
        };
        // Compared as an integer: a `G91.0` and a `G91` are the same word, and
        // matching on a float literal is a pattern Rust is right to dislike.
        let number = number.and_then(|n| (n.fract() == 0.0).then_some(n as i64));
        match (letter, number) {
            ('G', Some(90)) => axes_relative = false,
            ('G', Some(91)) => axes_relative = true,
            ('M', Some(82)) => e_relative = false,
            ('M', Some(83)) => e_relative = true,
            // Homing is the one button on the movement panel that is not a
            // direction, which makes it the trigger.
            ('G', Some(28)) => holds.push(tap(Key::Fire)),
            ('G', Some(0 | 1)) => {
                for (axis, delta) in move_words(&words) {
                    let relative = match axis {
                        Axis::E => e_relative,
                        _ => axes_relative,
                    };
                    // An absolute move names a destination, not a direction.
                    // Reading `G1 Y5` as "walk forward five" when the client
                    // meant "go to Y=5" would turn a move towards the front of
                    // the bed into a step backwards half the time.
                    if !relative {
                        continue;
                    }
                    if let Some(hold) = hold_for_move(axis, delta) {
                        holds.push(hold);
                    }
                }
            }
            _ => {}
        }
    }
    holds
}

/// The key an axis move presses, and for how long.
fn hold_for_move(axis: Axis, delta: f64) -> Option<Hold> {
    if !delta.is_finite() || delta == 0.0 {
        return None;
    }
    let key = match (axis, delta > 0.0) {
        // Y and Z are inverted; X is not. The printer's frame of reference is
        // the machine's, not the player's — a growing Y carries the bed
        // *towards* the operator, so the scene comes at you and the player
        // walks backwards; Z raises the toolhead, and up-on-the-panel reads as
        // sidestepping the other way once you are inside the game. Turning is
        // the exception, because the panel's left really is the player's left.
        // Established at the machine, one axis at a time.
        (Axis::Y, true) => Key::Back,
        (Axis::Y, false) => Key::Forward,
        // X turns rather than strafes: with only four movement buttons,
        // turning is what makes a corridor navigable at all.
        (Axis::X, true) => Key::TurnRight,
        (Axis::X, false) => Key::TurnLeft,
        (Axis::Z, true) => Key::StrafeLeft,
        (Axis::Z, false) => Key::StrafeRight,
        // Pushing filament out is firing; pulling it back is the use key, so
        // the filament panel is a second trigger and a door opener.
        (Axis::E, true) => Key::Fire,
        (Axis::E, false) => Key::Use,
    };
    // Clamped to the largest jog the rest of the crate will send, so a client
    // asking for a metre of travel is a long stride and not a minute of
    // walking with no way to stop it.
    let mm = delta.abs().min(MAX_JOG_MM);
    let ms = ((mm * MS_PER_MM as f64) as u64).max(MIN_HOLD_MS);
    Some(Hold { key, ms })
}

/// The axis words of a move line, in the order they appear.
fn move_words(words: &[(char, Option<f64>)]) -> Vec<(Axis, f64)> {
    words
        .iter()
        .filter_map(|(letter, number)| {
            let axis = match letter {
                'X' => Axis::X,
                'Y' => Axis::Y,
                'Z' => Axis::Z,
                'E' => Axis::E,
                _ => return None,
            };
            Some((axis, (*number)?))
        })
        .collect()
}

/// Split an uppercased line into `(letter, number)` words.
///
/// Tolerant of the forms a real sender uses — `G1 X10`, `G1X10`, a trailing
/// feedrate, a line number — for the same reason [`crate::core::safety`] is:
/// the payload comes from someone else's slicer, and a mapping that only
/// recognises one spelling is a button that works on one client.
fn words(body: &str) -> Vec<(char, Option<f64>)> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let letter = chars[i];
        if !letter.is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        // A space between the letter and its number is legal and does happen.
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        let start = j;
        while j < chars.len() && (chars[j].is_ascii_digit() || matches!(chars[j], '.' | '-' | '+'))
        {
            j += 1;
        }
        let number: Option<f64> = chars[start..j].iter().collect::<String>().parse().ok();
        out.push((letter, number));
        i = j.max(i + 1);
    }
    out
}

/// The keys that are down, and when to let go of them.
///
/// A button press is a press *and* a release later, and the release has to
/// happen even if the client never sends anything again — otherwise the player
/// walks into a wall forever. This holds the deadlines; the caller supplies the
/// clock and asks [`expire`](Self::expire) when [`next_deadline`](Self::next_deadline)
/// has passed.
#[derive(Debug, Default)]
pub struct KeyPad {
    /// Keyed by the key itself, so pressing the same button twice extends the
    /// hold rather than queueing a release in the middle of it.
    held: BTreeMap<Key, u64>,
}

impl KeyPad {
    pub fn new() -> Self {
        Self::default()
    }

    /// Press `hold` at `now_ms`, returning what to send the engine.
    ///
    /// A key already down is *not* pressed again: DOOM would see a second
    /// key-down, which for a weapon or the trigger means a second action the
    /// client never asked for. The deadline is extended instead, and only ever
    /// forwards — a short tap arriving during a long stride must not cut it
    /// short.
    pub fn press(&mut self, now_ms: u64, hold: Hold) -> Vec<Event> {
        let until = now_ms.saturating_add(hold.ms);
        match self.held.get_mut(&hold.key) {
            Some(existing) => {
                *existing = (*existing).max(until);
                Vec::new()
            }
            None => {
                self.held.insert(hold.key, until);
                vec![Event {
                    key: hold.key,
                    pressed: true,
                }]
            }
        }
    }

    /// Release everything whose hold has run out by `now_ms`.
    pub fn expire(&mut self, now_ms: u64) -> Vec<Event> {
        let done: Vec<Key> = self
            .held
            .iter()
            .filter(|(_, until)| **until <= now_ms)
            .map(|(key, _)| *key)
            .collect();
        for key in &done {
            self.held.remove(key);
        }
        done.into_iter()
            .map(|key| Event {
                key,
                pressed: false,
            })
            .collect()
    }

    /// When the next release is due, if anything is down.
    pub fn next_deadline(&self) -> Option<u64> {
        self.held.values().copied().min()
    }

    /// Let go of everything — for when the engine is going away and a key left
    /// down would be the state a restarted one inherits.
    pub fn release_all(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.held)
            .into_keys()
            .map(|key| Event {
                key,
                pressed: false,
            })
            .collect()
    }

    /// How many keys are down. For tests and diagnostics.
    pub fn held(&self) -> usize {
        self.held.len()
    }
}

// ---- the game, reported as a printer ------------------------------------

/// How the player is doing, as the engine last said.
///
/// `None` means the engine has not said — it is at the title screen, or it is
/// some other program that only draws pictures. Not the same as zero, which is
/// a dead player, and the difference decides whether the relay touches the
/// report at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vitals {
    pub health: Option<i16>,
    pub armour: Option<i16>,
}

/// The nozzle temperature that stands for full health.
///
/// 220 °C is what the captured A1 mini reports as its target for PLA, so a
/// client showing "220" is showing a number it would show during a real print.
/// The point of putting health here is that it needs no explanation: a printer
/// client already renders the nozzle in the largest type on the screen.
const PRINTING_C: f64 = 220.0;

/// The nozzle temperature that stands for none.
///
/// Room temperature, not zero: a machine that has been off all night reports
/// about this, and 0 °C is a reading no printer in a heated room ever gives. A
/// dead player is a cold nozzle, which is both the joke and a plausible number.
const AMBIENT_C: f64 = 25.0;

/// The bed temperature that stands for full armour, and none.
///
/// 60 °C is the captured machine's PLA bed target. Armour goes on the bed
/// because it is the other number a client shows next to the nozzle, and
/// because a second bar wants a second readout. Ammo deliberately goes
/// nowhere: there is no field on a printer's face where a count of bullets
/// reads as anything but a wrong number, and the game already draws it.
const BED_C: f64 = 60.0;

/// Full health and full armour, in DOOM's own units. Both go to 200 with a
/// sphere; 100 is what the game calls full and what the status bar shows as
/// `100%`.
const FULL: f64 = 100.0;

/// The nozzle temperature for `health`.
///
/// Linear from [`AMBIENT_C`] at zero to [`PRINTING_C`] at 100. A soulsphere
/// takes health past 100 and the nozzle past its printing temperature — the
/// player is running hot, which is exactly right — but not past the ceiling
/// [`crate::core::safety`] puts on a nozzle, because a client should never be
/// shown a temperature this crate would refuse to command.
pub fn nozzle_for_health(health: i16) -> f64 {
    let ceiling = TempLimits::default().max_nozzle;
    (AMBIENT_C + f64::from(health.max(0)) / FULL * (PRINTING_C - AMBIENT_C)).min(ceiling)
}

/// The bed temperature for `armour`, on the same scale and with the same cap.
pub fn bed_for_armour(armour: i16) -> f64 {
    let ceiling = TempLimits::default().max_bed;
    (AMBIENT_C + f64::from(armour.max(0)) / FULL * (BED_C - AMBIENT_C)).min(ceiling)
}

/// Put the player's state into a report on its way to a client.
///
/// The targets are set as well as the readings, and to the *full-health*
/// values rather than to anything the game is doing: a client draws current
/// against target, so "148 / 220" is a health bar without the client having
/// been told it is one.
///
/// Only a `push_status` is touched. An ACK wears the same envelope, and a
/// temperature spliced into a command's answer would be a reading in a place
/// nothing reads — the same rule [`crate::core::camerad::claim_camera`] follows,
/// for the same reason.
///
/// Returns whether anything changed.
pub fn report_vitals(message: &mut Value, vitals: Vitals) -> bool {
    if vitals == Vitals::default() {
        return false;
    }
    // `is_status_report` also passes a `get_version` inventory, which has no
    // `print` object to write into.
    if message.get("print").is_none() || !is_status_report(message) {
        return false;
    }
    let Some(print) = message.get_mut("print").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    // Written into every report, delta or snapshot, whether or not the printer
    // mentioned temperature: a health bar that only moved when the machine
    // happened to talk about its nozzle would lag the game by seconds.
    for (key, value) in [
        ("nozzle_temper", vitals.health.map(nozzle_for_health)),
        ("nozzle_target_temper", vitals.health.map(|_| PRINTING_C)),
        ("bed_temper", vitals.armour.map(bed_for_armour)),
        ("bed_target_temper", vitals.armour.map(|_| BED_C)),
    ] {
        let Some(value) = value else { continue };
        let value = Value::from((value * 10.0).round() / 10.0);
        if print.insert(key.to_string(), value.clone()) != Some(value) {
            changed = true;
        }
    }
    changed
}

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
    use serde_json::json;

    /// The payload this crate's own dashboard sends to jog an axis, which is
    /// also the shape docs/protocol.md records as accepted by the A1.
    fn jog(axis: &str, delta: f64) -> Value {
        json!({"print": {
            "sequence_id": "1",
            "command": "gcode_line",
            "param": format!("G91\nG1 {axis}{delta} F3000\nG90"),
        }})
    }

    fn keys(payload: &Value) -> Vec<Key> {
        holds_for(payload).into_iter().map(|h| h.key).collect()
    }

    // ---- the mapping -----------------------------------------------------

    #[test]
    fn the_bed_axis_walks_and_the_other_one_turns() {
        // The whole game is reachable from the four arrows on Studio's movement
        // panel, so these four are what the demo lives or dies by.
        assert_eq!(keys(&jog("Y", 10.0)), vec![Key::Back]);
        assert_eq!(keys(&jog("Y", -10.0)), vec![Key::Forward]);
        assert_eq!(keys(&jog("X", 10.0)), vec![Key::TurnRight]);
        assert_eq!(keys(&jog("X", -10.0)), vec![Key::TurnLeft]);
        assert_eq!(keys(&jog("Z", 10.0)), vec![Key::StrafeLeft]);
        assert_eq!(keys(&jog("Z", -10.0)), vec![Key::StrafeRight]);
    }

    #[test]
    fn the_home_button_is_the_trigger() {
        for line in ["G28", "G28 X", "G28 Z", " g28 "] {
            let payload = json!({"print": {"command": "gcode_line", "param": line}});
            assert_eq!(keys(&payload), vec![Key::Fire], "{line:?}");
        }
    }

    #[test]
    fn the_chamber_light_opens_doors_whichever_way_it_is_flipped() {
        // A switch that only works on the way on would be half a button, and
        // Studio's light control is a toggle: the client decides which edge it
        // is sending, and both mean "the operator pressed it".
        for mode in ["on", "off"] {
            let payload = json!({"system": {
                "sequence_id": "1", "command": "ledctrl",
                "led_node": "chamber_light", "led_mode": mode,
            }});
            assert_eq!(keys(&payload), vec![Key::Use], "{mode}");
        }
    }

    #[test]
    fn the_four_speed_levels_are_the_four_weapon_slots() {
        for slot in 1..=4u8 {
            let payload = json!({"print": {"command": "print_speed", "param": slot.to_string()}});
            assert_eq!(keys(&payload), vec![Key::Weapon(slot)], "level {slot}");
        }
        // The printer has four levels; anything else is not a slot, and
        // pressing some other ASCII digit at DOOM would select a weapon the
        // client never asked for.
        for bogus in ["0", "5", "", "sport"] {
            let payload = json!({"print": {"command": "print_speed", "param": bogus}});
            assert!(keys(&payload).is_empty(), "{bogus:?}");
        }
    }

    #[test]
    fn a_weapon_key_is_the_ascii_digit_doom_reads() {
        assert_eq!(Key::Weapon(1).code(), b'1');
        assert_eq!(Key::Weapon(4).code(), b'4');
    }

    #[test]
    fn the_job_buttons_reach_the_pause_and_the_menu() {
        let cmd = |c: &str| json!({"print": {"sequence_id": "1", "command": c}});
        assert_eq!(keys(&cmd("pause")), vec![Key::Pause]);
        assert_eq!(keys(&cmd("resume")), vec![Key::Pause]);
        assert_eq!(keys(&cmd("stop")), vec![Key::Escape]);
    }

    #[test]
    fn a_command_with_no_button_behind_it_presses_nothing() {
        // Consumed, not refused: a client is free to send whatever it likes,
        // and a printer that is secretly a game should be no harder to talk to
        // than one that is not.
        for payload in [
            json!({"print": {"command": "project_file", "url": "ftp:///model/x.3mf"}}),
            json!({"print": {"command": "clean_print_error"}}),
            json!({"system": {"command": "reboot"}}),
            json!({"print": {"command": "gcode_line", "param": "M104 S210"}}),
            json!({"print": {"command": "gcode_line"}}),
            json!({}),
        ] {
            assert!(keys(&payload).is_empty(), "{payload}");
        }
    }

    #[test]
    fn a_multi_axis_move_presses_both_keys() {
        let payload = json!({"print": {
            "command": "gcode_line", "param": "G91\nG1 X10 Y-10 F3000\nG90",
        }});
        assert_eq!(keys(&payload), vec![Key::TurnRight, Key::Forward]);
    }

    #[test]
    fn the_spellings_a_real_sender_uses_all_arrive() {
        // A mapping that only recognises one spelling is a button that works on
        // one client. Lowercase, no space before the number, a line number, a
        // trailing comment, an explicit plus.
        for param in [
            "g91\ng1 y10 f3000",
            "G91\nG1Y10F3000",
            "N5 G91\nN6 G1 Y10 ; step",
            "G91\nG1 Y+10",
            "G91\r\nG1 Y10\r\nG90",
        ] {
            let payload = json!({"print": {"command": "gcode_line", "param": param}});
            assert_eq!(keys(&payload), vec![Key::Back], "{param:?}");
        }
    }

    #[test]
    fn an_absolute_move_is_a_destination_and_presses_nothing() {
        // `G90` then `G1 Y5` means "go to Y=5", which says nothing about which
        // way the head travels — reading it as "forward five" would be a step
        // backwards half the time. Jogs are relative; this is not a jog.
        let payload = json!({"print": {"command": "gcode_line", "param": "G90\nG1 Y5 F3000"}});
        assert!(keys(&payload).is_empty());
        // …and the mode is per-payload: the trailing G90 of a jog restores
        // absolute mode *after* the move, which must still count.
        assert_eq!(keys(&jog("Y", 5.0)), vec![Key::Back]);
    }

    #[test]
    fn extruding_fires_and_retracting_uses() {
        // The filament panel sends `M83` (relative extrusion) around its move,
        // which is a different modal flag from G90/G91.
        let extrude = json!({"print": {
            "command": "gcode_line", "param": "M83\nG1 E10 F300\nM82",
        }});
        assert_eq!(keys(&extrude), vec![Key::Fire]);
        let retract = json!({"print": {
            "command": "gcode_line", "param": "M83\nG1 E-10 F300\nM82",
        }});
        assert_eq!(keys(&retract), vec![Key::Use]);
    }

    #[test]
    fn a_zero_move_presses_nothing() {
        // The G-code a slicer emits is full of `G1 F3000` and `G1 X0`; neither
        // is a button.
        for param in ["G91\nG1 X0 F3000", "G91\nG1 F3000", "G91\nG1"] {
            let payload = json!({"print": {"command": "gcode_line", "param": param}});
            assert!(keys(&payload).is_empty(), "{param:?}");
        }
    }

    // ---- how long a press lasts ------------------------------------------

    #[test]
    fn the_distance_asked_for_is_how_long_the_key_is_held() {
        // This is what makes the demo playable rather than a twitch: Studio's
        // 1 mm and 10 mm buttons have to feel like a nudge and a stride.
        let ms = |mm: f64| holds_for(&jog("Y", mm))[0].ms;
        assert!(
            ms(10.0) > ms(1.0),
            "10 mm should be a longer press than 1 mm"
        );
        assert_eq!(ms(10.0), 250, "10 mm at 25 ms/mm");
    }

    #[test]
    fn even_the_smallest_jog_is_held_long_enough_to_be_seen() {
        // DOOM samples the keyboard once a tic (~28 ms). A press and release
        // inside one tic never appears down at all, so a 0.1 mm jog — which is
        // 2.5 ms of gearing — would silently do nothing.
        assert!(holds_for(&jog("Y", 0.1))[0].ms >= 2 * TIC_MS);
        // And so is a press with no distance behind it.
        assert!(
            holds_for(&json!({"print": {"command": "gcode_line", "param": "G28"}}))[0].ms
                >= 2 * TIC_MS
        );
    }

    #[test]
    fn an_enormous_jog_is_a_long_stride_and_not_a_minute_of_walking() {
        // Nothing stops a client asking for a metre. Held literally, that is
        // 25 seconds of the player marching into a wall with the operator
        // unable to interrupt it.
        let huge = holds_for(&jog("Y", 100_000.0))[0].ms;
        assert_eq!(huge, MAX_JOG_MM as u64 * MS_PER_MM);
    }

    #[test]
    fn a_nonsense_distance_presses_nothing() {
        for param in ["G91\nG1 Ynan", "G91\nG1 Y-", "G91\nG1 Y..."] {
            let payload = json!({"print": {"command": "gcode_line", "param": param}});
            assert!(keys(&payload).is_empty(), "{param:?}");
        }
    }

    // ---- the wire --------------------------------------------------------

    #[test]
    fn an_event_is_the_two_bytes_dg_getkey_hands_back() {
        assert_eq!(
            Event {
                key: Key::Forward,
                pressed: true
            }
            .to_wire(),
            [1, 0xad]
        );
        assert_eq!(
            Event {
                key: Key::Forward,
                pressed: false
            }
            .to_wire(),
            [0, 0xad]
        );
    }

    #[test]
    fn the_key_codes_are_doomkeys_hs_own() {
        // Pinned against the header rather than derived, because a wrong code
        // is a button that silently does nothing — the hardest failure here to
        // tell from a mapping that was never reached.
        assert_eq!(Key::Forward.code(), 0xad);
        assert_eq!(Key::Back.code(), 0xaf);
        assert_eq!(Key::TurnLeft.code(), 0xac);
        assert_eq!(Key::TurnRight.code(), 0xae);
        assert_eq!(Key::StrafeLeft.code(), 0xa0);
        assert_eq!(Key::StrafeRight.code(), 0xa1);
        assert_eq!(Key::Use.code(), 0xa2);
        assert_eq!(Key::Fire.code(), 0xa3);
        assert_eq!(Key::Escape.code(), 27);
        assert_eq!(Key::Pause.code(), 0xff);
    }

    // ---- holding and letting go ------------------------------------------

    #[test]
    fn a_press_is_sent_now_and_the_release_when_the_hold_runs_out() {
        let mut pad = KeyPad::new();
        let events = pad.press(
            1_000,
            Hold {
                key: Key::Forward,
                ms: 250,
            },
        );
        assert_eq!(
            events,
            vec![Event {
                key: Key::Forward,
                pressed: true
            }]
        );
        assert_eq!(pad.next_deadline(), Some(1_250));
        assert!(pad.expire(1_249).is_empty(), "not yet");
        assert_eq!(
            pad.expire(1_250),
            vec![Event {
                key: Key::Forward,
                pressed: false
            }]
        );
        assert_eq!(pad.next_deadline(), None);
    }

    #[test]
    fn pressing_a_held_key_again_extends_it_instead_of_pressing_twice() {
        // Two key-downs with no release between them is a second action the
        // client never asked for — a second shot, or a weapon switch. And a
        // release scheduled from the first press must not cut the second short.
        let mut pad = KeyPad::new();
        pad.press(
            0,
            Hold {
                key: Key::Forward,
                ms: 250,
            },
        );
        let again = pad.press(
            100,
            Hold {
                key: Key::Forward,
                ms: 250,
            },
        );
        assert!(again.is_empty(), "already down");
        assert!(pad.expire(250).is_empty(), "the second press extended it");
        assert_eq!(pad.next_deadline(), Some(350));
        assert_eq!(pad.expire(350).len(), 1);
    }

    #[test]
    fn a_short_press_during_a_long_one_does_not_shorten_it() {
        // The deadline only ever moves forwards. Tapping 1 mm while a 10 mm
        // stride is under way would otherwise stop the stride early.
        let mut pad = KeyPad::new();
        pad.press(
            0,
            Hold {
                key: Key::Forward,
                ms: 1_000,
            },
        );
        pad.press(
            10,
            Hold {
                key: Key::Forward,
                ms: 60,
            },
        );
        assert_eq!(pad.next_deadline(), Some(1_000));
    }

    #[test]
    fn two_keys_are_held_independently() {
        let mut pad = KeyPad::new();
        pad.press(
            0,
            Hold {
                key: Key::Forward,
                ms: 500,
            },
        );
        pad.press(
            0,
            Hold {
                key: Key::TurnRight,
                ms: 100,
            },
        );
        assert_eq!(pad.held(), 2);
        assert_eq!(
            pad.expire(100),
            vec![Event {
                key: Key::TurnRight,
                pressed: false
            }]
        );
        assert_eq!(pad.held(), 1, "the stride is still going");
    }

    // ---- the game, reported as a printer ---------------------------------

    /// A report shaped like the printer's own, with the fields the overlay
    /// writes into already present and wrong.
    fn a_report() -> Value {
        json!({"print": {
            "command": "push_status", "msg": 0,
            "nozzle_temper": 24.0, "nozzle_target_temper": 0.0,
            "bed_temper": 24.0, "bed_target_temper": 0.0,
            "gcode_state": "IDLE",
        }})
    }

    #[test]
    fn full_health_is_a_temperature_a_printer_really_prints_at() {
        // The whole joke depends on the number being unremarkable: someone
        // glancing at Studio should see a printer at 220 °C, not a fault.
        assert_eq!(nozzle_for_health(100), 220.0);
        // And nothing is a cold machine, not a broken sensor.
        assert_eq!(nozzle_for_health(0), 25.0);
        // Halfway is halfway.
        assert!((nozzle_for_health(50) - 122.5).abs() < 0.01);
        assert_eq!(bed_for_armour(100), 60.0);
        assert_eq!(bed_for_armour(0), 25.0);
    }

    #[test]
    fn health_only_ever_goes_up_with_health() {
        let mut last = f64::MIN;
        for health in 0..=200 {
            let now = nozzle_for_health(health);
            assert!(now >= last, "{health} went backwards: {now} after {last}");
            last = now;
        }
    }

    #[test]
    fn a_soulsphere_runs_hot_but_never_past_what_this_crate_would_command() {
        // Health goes to 200, and 200 on this scale is 415 °C — a number no
        // printer shows and this crate's own safety layer would refuse to send.
        // Above full health the nozzle should read hot, not broken.
        let limits = TempLimits::default();
        assert!(nozzle_for_health(150) > 220.0, "a sphere should run hot");
        assert_eq!(nozzle_for_health(200), limits.max_nozzle);
        assert_eq!(nozzle_for_health(i16::MAX), limits.max_nozzle);
        assert!(bed_for_armour(i16::MAX) <= limits.max_bed);
        // A negative reading cannot come out of `parse_vitals`, but the mapping
        // must not produce a nonsense temperature if one ever does.
        assert_eq!(nozzle_for_health(-50), 25.0);
    }

    #[test]
    fn a_report_carries_the_players_health_as_the_nozzle() {
        let mut report = a_report();
        assert!(report_vitals(
            &mut report,
            Vitals {
                health: Some(100),
                armour: Some(50),
            }
        ));
        assert_eq!(report["print"]["nozzle_temper"], 220.0);
        assert_eq!(report["print"]["bed_temper"], 42.5);
        // The targets are the FULL-health values, not the game's: a client
        // draws current against target, so this is a health bar it was never
        // told about.
        assert_eq!(report["print"]["nozzle_target_temper"], 220.0);
        assert_eq!(report["print"]["bed_target_temper"], 60.0);
        // Everything else is the printer's to say.
        assert_eq!(report["print"]["gcode_state"], "IDLE");
    }

    #[test]
    fn a_delta_that_never_mentioned_temperature_carries_it_anyway() {
        // Health has to move when the player is hit, not when the printer next
        // happens to talk about its nozzle.
        let mut delta = json!({"print": {"command": "push_status", "msg": 1, "mc_percent": 4}});
        assert!(report_vitals(
            &mut delta,
            Vitals {
                health: Some(75),
                armour: None,
            }
        ));
        assert_eq!(delta["print"]["nozzle_temper"], 171.3);
        assert!(
            delta["print"].get("bed_temper").is_none(),
            "armour was unknown; the bed is the printer's own"
        );
    }

    #[test]
    fn nothing_is_written_when_the_game_has_not_said() {
        // An engine at the title screen, or one that is not a game at all. The
        // printer's own readings must survive that.
        let mut report = a_report();
        assert!(!report_vitals(&mut report, Vitals::default()));
        assert_eq!(report, a_report());
    }

    #[test]
    fn an_ack_is_not_a_place_to_report_a_temperature() {
        // An ACK wears the same envelope as a report. A nozzle reading spliced
        // into a command's answer is a number where nothing reads one — and it
        // would travel into the cache and out again with every snapshot.
        for mut message in [
            json!({"print": {"sequence_id": "1", "param": "G28", "result": "success"}}),
            json!({"system": {"sequence_id": "1", "result": "success"}}),
            json!({"info": {"command": "get_version", "module": []}}),
        ] {
            let before = message.clone();
            assert!(
                !report_vitals(
                    &mut message,
                    Vitals {
                        health: Some(100),
                        armour: Some(100)
                    }
                ),
                "{before}"
            );
            assert_eq!(message, before);
        }
    }

    #[test]
    fn writing_the_same_reading_twice_changes_nothing() {
        // The caller re-encodes a message it was told changed; a health bar
        // that has not moved should not make work.
        let vitals = Vitals {
            health: Some(80),
            armour: Some(20),
        };
        let mut report = a_report();
        assert!(report_vitals(&mut report, vitals));
        assert!(!report_vitals(&mut report, vitals));
    }

    // ---- the engine's status record --------------------------------------

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

    #[test]
    fn everything_is_let_go_of_when_the_engine_goes_away() {
        // A key left down is the state a restarted engine would inherit, and
        // the player would be walking before anyone pressed anything.
        let mut pad = KeyPad::new();
        pad.press(
            0,
            Hold {
                key: Key::Forward,
                ms: 10_000,
            },
        );
        pad.press(
            0,
            Hold {
                key: Key::Fire,
                ms: 10_000,
            },
        );
        let released = pad.release_all();
        assert_eq!(released.len(), 2);
        assert!(released.iter().all(|e| !e.pressed));
        assert_eq!(pad.held(), 0);
        assert_eq!(pad.next_deadline(), None);
    }
}
