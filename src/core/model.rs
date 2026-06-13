//! Printer models.
//!
//! For the MVP the model is supplied by the user in their profile
//! (`--model a1mini`); device-side detection from a `pushall` report is added
//! once we have real captures to derive the mapping from.

use std::fmt;

/// A Bambu Lab printer model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Model {
    A1Mini,
    A1,
    P1P,
    P1S,
    X1Carbon,
    X1E,
    H2D,
    /// A model name we don't recognise, kept verbatim.
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
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_names() {
        assert_eq!(Model::from_config_str("a1mini"), Model::A1Mini);
        assert_eq!(Model::from_config_str("a1"), Model::A1);
        assert_eq!(Model::from_config_str("p1s"), Model::P1S);
        assert_eq!(Model::from_config_str("x1c"), Model::X1Carbon);
        assert_eq!(Model::from_config_str("h2d"), Model::H2D);
    }

    #[test]
    fn parsing_is_lenient_about_case_and_separators() {
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
    fn known_models_round_trip_through_canonical_name() {
        for m in [
            Model::A1Mini,
            Model::A1,
            Model::P1P,
            Model::P1S,
            Model::X1Carbon,
            Model::X1E,
            Model::H2D,
        ] {
            assert!(m.is_known());
            assert_eq!(Model::from_config_str(m.as_str()), m);
        }
    }
}
