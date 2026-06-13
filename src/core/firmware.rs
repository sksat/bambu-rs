//! Printer firmware versions.
//!
//! Bambu firmware versions look like `01.05.00.00` (dot-separated, usually
//! zero-padded two-digit components). The capability registry keys on
//! `(model, FirmwareVersion)`, so we need a type that parses these strings and
//! orders them correctly — `01.04.99.00 < 01.05.00.00`, and trailing-zero
//! components are insignificant (`01.05` == `01.05.00.00`).

use std::cmp::Ordering;
use std::fmt;

/// A parsed, comparable Bambu firmware version.
///
/// Equality and ordering are defined on the *normalised* numeric components
/// (trailing zero components dropped), so `01.05` and `01.05.00.00` compare
/// equal. The original string is preserved for [`fmt::Display`].
#[derive(Debug, Clone)]
pub struct FirmwareVersion {
    /// Original input (e.g. `"01.05.00.00"`), preserved for display.
    raw: String,
    /// Numeric components with insignificant trailing zeros removed.
    components: Vec<u32>,
}

/// Error returned when a firmware version string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFirmwareError {
    input: String,
}

impl fmt::Display for ParseFirmwareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid firmware version: {:?}", self.input)
    }
}

impl std::error::Error for ParseFirmwareError {}

impl FirmwareVersion {
    /// Parse a firmware version string such as `"01.05.00.00"` (an optional
    /// leading `v`/`V` is accepted).
    pub fn parse(s: &str) -> Result<Self, ParseFirmwareError> {
        let raw = s.trim();
        let err = || ParseFirmwareError {
            input: s.to_string(),
        };
        let body = raw.strip_prefix(['v', 'V']).unwrap_or(raw);
        if body.is_empty() {
            return Err(err());
        }
        let mut components = body
            .split('.')
            .map(|part| part.parse::<u32>().map_err(|_| err()))
            .collect::<Result<Vec<u32>, _>>()?;
        // Drop insignificant trailing zeros so `01.05` == `01.05.00.00`.
        while components.last() == Some(&0) {
            components.pop();
        }
        Ok(Self {
            raw: raw.to_string(),
            components,
        })
    }

    /// The normalised numeric components (trailing zeros removed).
    pub fn components(&self) -> &[u32] {
        &self.components
    }
}

impl PartialEq for FirmwareVersion {
    fn eq(&self, other: &Self) -> bool {
        self.components == other.components
    }
}

impl Eq for FirmwareVersion {}

impl PartialOrd for FirmwareVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FirmwareVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        // Normalised components have no trailing zeros, so lexicographic
        // comparison of the `Vec<u32>` matches numeric version ordering.
        self.components.cmp(&other.components)
    }
}

impl std::hash::Hash for FirmwareVersion {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.components.hash(state);
    }
}

impl serde::Serialize for FirmwareVersion {
    /// Serialise as the original version string (e.g. `"01.07.02.00"`), so JSON
    /// output round-trips what the device reported.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw)
    }
}

impl fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl std::str::FromStr for FirmwareVersion {
    type Err = ParseFirmwareError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fw(s: &str) -> FirmwareVersion {
        FirmwareVersion::parse(s).expect("should parse")
    }

    #[test]
    fn parses_four_component_version() {
        let v = fw("01.05.00.00");
        assert_eq!(v.components(), &[1, 5]); // trailing zeros are insignificant
    }

    #[test]
    fn accepts_optional_v_prefix() {
        assert_eq!(fw("v01.05.00"), fw("01.05.00"));
        assert_eq!(fw("V01.05.00"), fw("01.05.00"));
    }

    #[test]
    fn trailing_zero_components_are_equal() {
        assert_eq!(fw("01.05"), fw("01.05.00.00"));
        assert_eq!(fw("01.05.00"), fw("01.05.00.00"));
    }

    #[test]
    fn orders_by_numeric_component() {
        assert!(fw("01.04.99.00") < fw("01.05.00.00"));
        assert!(fw("01.05.00.00") < fw("01.05.01.00"));
        assert!(fw("01.05.00.00") < fw("01.05.00.02"));
        assert!(fw("01.05.01") > fw("01.05.00.02"));
    }

    #[test]
    fn developer_mode_threshold_comparison() {
        // A1: Developer Mode requires firmware >= 01.05.00.
        let threshold = fw("01.05.00");
        assert!(fw("01.04.00.00") < threshold);
        assert!(fw("01.05.00.00") >= threshold);
        assert!(fw("01.06.02.00") >= threshold);
    }

    #[test]
    fn display_round_trips_original_string() {
        assert_eq!(fw("01.05.00.00").to_string(), "01.05.00.00");
    }

    #[test]
    fn rejects_non_numeric() {
        assert!(FirmwareVersion::parse("not-a-version").is_err());
        assert!(FirmwareVersion::parse("01.x.00").is_err());
        assert!(FirmwareVersion::parse("").is_err());
    }
}
