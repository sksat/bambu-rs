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

use std::collections::BTreeMap;

use serde_json::Value;

use crate::core::safety::MAX_JOG_MM;

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
        // Y is the bed moving towards and away from you on an A1, which is as
        // close to "forward" as a printer gets.
        (Axis::Y, true) => Key::Forward,
        (Axis::Y, false) => Key::Back,
        // X turns rather than strafes: with only four movement buttons,
        // turning is what makes a corridor navigable at all.
        (Axis::X, true) => Key::TurnRight,
        (Axis::X, false) => Key::TurnLeft,
        (Axis::Z, true) => Key::StrafeRight,
        (Axis::Z, false) => Key::StrafeLeft,
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
        assert_eq!(keys(&jog("Y", 10.0)), vec![Key::Forward]);
        assert_eq!(keys(&jog("Y", -10.0)), vec![Key::Back]);
        assert_eq!(keys(&jog("X", 10.0)), vec![Key::TurnRight]);
        assert_eq!(keys(&jog("X", -10.0)), vec![Key::TurnLeft]);
        assert_eq!(keys(&jog("Z", 10.0)), vec![Key::StrafeRight]);
        assert_eq!(keys(&jog("Z", -10.0)), vec![Key::StrafeLeft]);
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
        assert_eq!(keys(&payload), vec![Key::TurnRight, Key::Back]);
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
            assert_eq!(keys(&payload), vec![Key::Forward], "{param:?}");
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
        assert_eq!(keys(&jog("Y", 5.0)), vec![Key::Forward]);
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
