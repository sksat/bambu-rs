//! HMS (Health Management System) error decoding.
//!
//! Model-**independent**: the bit layout is identical across every Bambu model,
//! so this is plain `core` with no model gating (the one model-specific aspect —
//! XCAM/micro-LiDAR codes — is only *emitted* by X1-class hardware; callers can
//! check [`HmsEntry::is_lidar`] / the model's `lidar` capability).
//!
//! The report carries `hms` as an array of `{attr, code}` 32-bit integers (our
//! observed A1 mini reports `hms: []` = no active alerts). Each pair decodes to
//! `HMS_AAAA_BBBB_CCCC_DDDD` where `AAAA = attr>>16`, `BBBB = attr&0xFFFF`,
//! `CCCC = code>>16`, `DDDD = code&0xFFFF`.
//!
//! Provenance: the bit layout is reconstructed from protocol docs and
//! cross-checked against the official Bambu wiki (the worked example
//! `HMS_0300_0100_0001_0007` = "heatbed temperature abnormal" is confirmed). We
//! deliberately do **not** hardcode a severity-label table (sources conflict)
//! nor bundle any code→text mapping (that would copy pybambu's data); the code
//! string + a wiki URL are emitted instead.

use serde_json::Value;

/// One decoded HMS alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HmsEntry {
    pub attr: u32,
    pub code: u32,
}

/// The functional module an HMS code originates from — `(attr >> 24) & 0xFF`.
/// Only the well-corroborated modules are named; the community-only ids
/// (0x10/0x11/0x12/0x18) are left as [`Module::Unknown`] until verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Module {
    /// 0x03 — motion controller.
    MotionController,
    /// 0x05 — mainboard / AP.
    Mainboard,
    /// 0x07 — AMS.
    Ams,
    /// 0x08 — toolhead.
    Toolhead,
    /// 0x0C — XCAM / micro-LiDAR (only on X1-class hardware).
    Xcam,
    Unknown(u8),
}

impl HmsEntry {
    pub fn new(attr: u32, code: u32) -> Self {
        Self { attr, code }
    }

    fn groups(&self) -> [u16; 4] {
        [
            (self.attr >> 16) as u16,
            (self.attr & 0xFFFF) as u16,
            (self.code >> 16) as u16,
            (self.code & 0xFFFF) as u16,
        ]
    }

    /// Canonical code string, underscore form: `HMS_0300_0100_0001_0007`.
    pub fn code_string(&self) -> String {
        let [a, b, c, d] = self.groups();
        format!("HMS_{a:04X}_{b:04X}_{c:04X}_{d:04X}")
    }

    /// The four groups joined by hyphens (Bambu's on-screen form):
    /// `0300-0100-0001-0007`.
    pub fn code_hyphen(&self) -> String {
        let [a, b, c, d] = self.groups();
        format!("{a:04X}-{b:04X}-{c:04X}-{d:04X}")
    }

    /// Raw severity value = `code >> 16`. **Not** mapped to a label: sources
    /// conflict (pybambu 1=fatal/2=serious/3=common/4=info vs the Bambu wiki
    /// 1=Error/2=Warning/3=Info), so the bits are exposed and the label is left
    /// to the caller.
    pub fn severity_raw(&self) -> u16 {
        (self.code >> 16) as u16
    }

    /// The originating module — `(attr >> 24) & 0xFF`.
    pub fn module(&self) -> Module {
        match ((self.attr >> 24) & 0xFF) as u8 {
            0x03 => Module::MotionController,
            0x05 => Module::Mainboard,
            0x07 => Module::Ams,
            0x08 => Module::Toolhead,
            0x0C => Module::Xcam,
            other => Module::Unknown(other),
        }
    }

    /// Whether this is an XCAM/micro-LiDAR code (only emitted by X1-class).
    pub fn is_lidar(&self) -> bool {
        matches!(self.module(), Module::Xcam)
    }

    /// Best-effort link to Bambu's per-code troubleshooting page.
    pub fn wiki_url(&self) -> String {
        let [a, b, c, d] = self.groups();
        format!(
            "https://wiki.bambulab.com/en/x1/troubleshooting/hmscode/{a:04X}_{b:04X}_{c:04X}_{d:04X}"
        )
    }
}

/// Decode the `hms[]` array from a merged report state (`state["print"]["hms"]`).
/// Entries with a zero `attr` or `code` are skipped (inactive / padding).
pub fn decode_report_hms(state: &Value) -> Vec<HmsEntry> {
    let Some(arr) = state.pointer("/print/hms").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let attr = e.get("attr").and_then(Value::as_u64)?;
            let code = e.get("code").and_then(Value::as_u64)?;
            if attr == 0 || code == 0 {
                return None;
            }
            Some(HmsEntry::new(attr as u32, code as u32))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn worked_example_decodes_like_the_official_wiki() {
        // attr=0x03000100, code=0x00010007 -> HMS_0300_0100_0001_0007
        // ("heatbed temperature abnormal", confirmed against the Bambu wiki).
        let e = HmsEntry::new(0x0300_0100, 0x0001_0007);
        assert_eq!(e.code_string(), "HMS_0300_0100_0001_0007");
        assert_eq!(e.code_hyphen(), "0300-0100-0001-0007");
        assert_eq!(e.severity_raw(), 1);
        assert_eq!(e.module(), Module::MotionController);
        assert!(!e.is_lidar());
        assert!(e.wiki_url().ends_with("/hmscode/0300_0100_0001_0007"));
    }

    #[test]
    fn module_is_decoded_from_the_attr_high_byte() {
        assert_eq!(HmsEntry::new(0x0500_0000, 1).module(), Module::Mainboard);
        assert_eq!(HmsEntry::new(0x0700_0000, 1).module(), Module::Ams);
        assert_eq!(HmsEntry::new(0x0800_0000, 1).module(), Module::Toolhead);
        assert_eq!(
            HmsEntry::new(0x0A00_0000, 1).module(),
            Module::Unknown(0x0A)
        );
    }

    #[test]
    fn xcam_codes_are_flagged_as_lidar() {
        let e = HmsEntry::new(0x0C00_0100, 0x0003_0001);
        assert_eq!(e.module(), Module::Xcam);
        assert!(e.is_lidar());
    }

    #[test]
    fn observed_a1mini_fixture_has_no_active_alerts() {
        let raw = include_str!("../../tests/fixtures/pushall-n1-idle.json");
        let fixture: Value = serde_json::from_str(raw).unwrap();
        // hms: [] was observed on the real device.
        assert_eq!(decode_report_hms(&fixture["message"]), Vec::new());
    }

    #[test]
    fn decode_skips_zero_padding_entries() {
        let state = json!({ "print": { "hms": [
            { "attr": 0, "code": 0 },
            { "attr": 50331904, "code": 65543 }, // 0x03000100 / 0x00010007
            { "attr": 123, "code": 0 },
        ]}});
        let decoded = decode_report_hms(&state);
        assert_eq!(decoded, vec![HmsEntry::new(0x0300_0100, 0x0001_0007)]);
    }

    #[test]
    fn missing_hms_field_decodes_to_empty() {
        assert_eq!(decode_report_hms(&json!({ "print": {} })), Vec::new());
        assert_eq!(decode_report_hms(&json!({})), Vec::new());
    }
}
