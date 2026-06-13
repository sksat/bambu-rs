//! Printer models and their canonical device codes.
//!
//! The model can be supplied by the user (`--model a1mini`) or resolved from a
//! device-reported code (SSDP `DevModel.bambu.com`, the cloud `dev_model_name`,
//! or the MQTT module `project_name` — for most models these share one code
//! namespace).

use std::fmt;

/// A Bambu Lab printer model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Model {
    A1Mini,
    A1,
    P1P,
    P1S,
    X1,
    X1Carbon,
    X1E,
    H2D,
    /// A model name/code we don't recognise, kept verbatim.
    Unknown(String),
}

impl Model {
    /// Parse a user-facing config model name (case-insensitive, ignoring spaces,
    /// hyphens and underscores), e.g. `"a1mini"`, `"A1 mini"`, `"x1c"`.
    pub fn from_config_str(s: &str) -> Self {
        let normalized: String = s
            .chars()
            .filter(|c| !matches!(c, ' ' | '-' | '_'))
            .flat_map(char::to_lowercase)
            .collect();
        match normalized.as_str() {
            "a1mini" => Model::A1Mini,
            "a1" => Model::A1,
            "p1p" => Model::P1P,
            "p1s" => Model::P1S,
            "x1" => Model::X1,
            "x1c" | "x1carbon" => Model::X1Carbon,
            "x1e" => Model::X1E,
            "h2d" => Model::H2D,
            _ => Model::Unknown(s.trim().to_string()),
        }
    }

    /// The canonical config name (round-trips through [`Model::from_config_str`]).
    pub fn as_str(&self) -> &str {
        match self {
            Model::A1Mini => "a1mini",
            Model::A1 => "a1",
            Model::P1P => "p1p",
            Model::P1S => "p1s",
            Model::X1 => "x1",
            Model::X1Carbon => "x1c",
            Model::X1E => "x1e",
            Model::H2D => "h2d",
            Model::Unknown(s) => s,
        }
    }

    /// Whether this is a recognised model (not [`Model::Unknown`]).
    pub fn is_known(&self) -> bool {
        !matches!(self, Model::Unknown(_))
    }

    /// Resolve a device-reported model **code** to a [`Model`].
    ///
    /// These are the **vendor-canonical** codes from Bambu's own slicer machine
    /// list (`BambuStudio/resources/printers/<code>.json` `model_id`), which for
    /// the A1 family also match what the printer broadcasts over SSDP. We treat
    /// the *mapping* as fact (vendor source); we do not copy the profile
    /// contents. The A1 mini ↔ `"N1"` mapping is additionally **hardware-observed**
    /// on a real unit (2026-06-13) — note that the once-"common" `N2S = A1 mini`
    /// belief is inverted; `N1` is the A1 mini and `N2S` is the full-size A1.
    ///
    /// The legacy SSDP strings `"3DPrinter-X1-Carbon"` / `"3DPrinter-X1"` are
    /// accepted and normalised to the modern `BL-P001` / `BL-P002` models.
    pub fn from_device_code(code: &str) -> Self {
        match code.trim() {
            "N1" => Model::A1Mini,
            "N2S" => Model::A1,
            "C11" => Model::P1P,
            "C12" => Model::P1S,
            "C13" => Model::X1E,
            "BL-P001" | "3DPrinter-X1-Carbon" => Model::X1Carbon,
            "BL-P002" | "3DPrinter-X1" => Model::X1,
            "O1D" => Model::H2D,
            other => Model::Unknown(other.to_string()),
        }
    }

    /// The canonical device code for this model (round-trips through
    /// [`Model::from_device_code`]); `None` for [`Model::Unknown`].
    pub fn device_code(&self) -> Option<&'static str> {
        Some(match self {
            Model::A1Mini => "N1",
            Model::A1 => "N2S",
            Model::P1P => "C11",
            Model::P1S => "C12",
            Model::X1 => "BL-P002",
            Model::X1Carbon => "BL-P001",
            Model::X1E => "C13",
            Model::H2D => "O1D",
            Model::Unknown(_) => return None,
        })
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWN: [Model; 8] = [
        Model::A1Mini,
        Model::A1,
        Model::P1P,
        Model::P1S,
        Model::X1,
        Model::X1Carbon,
        Model::X1E,
        Model::H2D,
    ];

    #[test]
    fn parses_canonical_config_names() {
        assert_eq!(Model::from_config_str("a1mini"), Model::A1Mini);
        assert_eq!(Model::from_config_str("a1"), Model::A1);
        assert_eq!(Model::from_config_str("p1s"), Model::P1S);
        assert_eq!(Model::from_config_str("x1"), Model::X1);
        assert_eq!(Model::from_config_str("x1c"), Model::X1Carbon);
        assert_eq!(Model::from_config_str("h2d"), Model::H2D);
    }

    #[test]
    fn config_parsing_is_lenient_about_case_and_separators() {
        assert_eq!(Model::from_config_str("A1 mini"), Model::A1Mini);
        assert_eq!(Model::from_config_str("A1-Mini"), Model::A1Mini);
        assert_eq!(Model::from_config_str("a1_mini"), Model::A1Mini);
        assert_eq!(Model::from_config_str("X1Carbon"), Model::X1Carbon);
    }

    #[test]
    fn unknown_model_is_kept_verbatim() {
        let m = Model::from_config_str("z9 ultra");
        assert_eq!(m, Model::Unknown("z9 ultra".to_string()));
        assert!(!m.is_known());
    }

    #[test]
    fn resolves_vendor_canonical_device_codes() {
        assert_eq!(Model::from_device_code("N1"), Model::A1Mini); // hardware-observed
        assert_eq!(Model::from_device_code("N2S"), Model::A1); // NOT the A1 mini
        assert_eq!(Model::from_device_code("C11"), Model::P1P);
        assert_eq!(Model::from_device_code("C12"), Model::P1S);
        assert_eq!(Model::from_device_code("C13"), Model::X1E);
        assert_eq!(Model::from_device_code("BL-P001"), Model::X1Carbon);
        assert_eq!(Model::from_device_code("BL-P002"), Model::X1);
        assert_eq!(Model::from_device_code("O1D"), Model::H2D);
    }

    #[test]
    fn x1_and_x1_carbon_are_distinct_codes() {
        assert_ne!(Model::X1, Model::X1Carbon);
        assert_eq!(Model::X1.device_code(), Some("BL-P002"));
        assert_eq!(Model::X1Carbon.device_code(), Some("BL-P001"));
    }

    #[test]
    fn legacy_ssdp_strings_normalise_to_modern_models() {
        assert_eq!(
            Model::from_device_code("3DPrinter-X1-Carbon"),
            Model::X1Carbon
        );
        assert_eq!(Model::from_device_code("3DPrinter-X1"), Model::X1);
    }

    #[test]
    fn unrecognised_device_code_is_unknown() {
        assert_eq!(
            Model::from_device_code("ZZ9"),
            Model::Unknown("ZZ9".to_string())
        );
        assert_eq!(Model::Unknown("ZZ9".into()).device_code(), None);
    }

    #[test]
    fn device_codes_round_trip_for_all_known_models() {
        for m in KNOWN {
            let code = m.device_code().expect("known model has a device code");
            assert_eq!(Model::from_device_code(code), m, "round-trip for {m}");
        }
    }

    #[test]
    fn config_names_round_trip_for_all_known_models() {
        for m in KNOWN {
            assert!(m.is_known());
            assert_eq!(Model::from_config_str(m.as_str()), m);
        }
    }
}
