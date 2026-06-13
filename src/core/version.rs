//! Parsed `info.get_version` response — the printer's module/firmware inventory.
//!
//! `get_version` answers with a `module[]` array, one entry per hardware/software
//! component (`ota`, `esp32`, `mc`, `th`, `ams_f1/0`, …). The **`ota`** module's
//! `sw_ver` is the printer's user-facing firmware version — the value the
//! capability registry keys on. Shapes are from a **real A1 mini capture**
//! (`tests/fixtures/get_version-a1mini.json`).

use crate::core::firmware::FirmwareVersion;
use serde::Serialize;
use serde_json::Value;

/// One module reported by `get_version` (a hardware/software component).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Module {
    /// Module name, e.g. `ota`, `esp32`, `mc`, `ams_f1/0`.
    pub name: String,
    /// Hardware revision, e.g. `OTA`, `AP05`, `AMS_F102`.
    pub hw_ver: Option<String>,
    /// Software/firmware version of this module, e.g. `01.07.02.00`.
    pub sw_ver: Option<String>,
    /// Marketing name when the module carries one (`Bambu Lab A1 mini`,
    /// `AMS Lite`); empty strings are dropped to `None`.
    pub product_name: Option<String>,
}

impl Module {
    /// Parse one `module[]` entry; `None` when it has no `name`.
    fn from_value(v: &Value) -> Option<Module> {
        let name = v.get("name").and_then(Value::as_str)?.to_string();
        let nonempty = |key: &str| {
            v.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        Some(Module {
            name,
            hw_ver: nonempty("hw_ver"),
            sw_ver: nonempty("sw_ver"),
            product_name: nonempty("product_name"),
        })
    }
}

/// The printer's version inventory, parsed from an `info.get_version` response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceVersion {
    /// The OTA module's `sw_ver` as a parsed firmware version — the printer's
    /// user-facing firmware, which the capability registry keys on. `None` when
    /// there's no `ota` module or its `sw_ver` doesn't parse.
    pub firmware: Option<FirmwareVersion>,
    /// Every reported module, in report order.
    pub modules: Vec<Module>,
}

impl DeviceVersion {
    /// Parse from the object under `info` in a `get_version` response (the value
    /// holding `command: "get_version"` and the `module` array).
    pub fn from_info(info: &Value) -> Self {
        let modules: Vec<Module> = info
            .get("module")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Module::from_value).collect())
            .unwrap_or_default();
        let firmware = modules
            .iter()
            .find(|m| m.name == "ota")
            .and_then(|m| m.sw_ver.as_deref())
            .and_then(|s| FirmwareVersion::parse(s).ok());
        DeviceVersion { firmware, modules }
    }

    /// Find a module by exact name.
    pub fn module(&self, name: &str) -> Option<&Module> {
        self.modules.iter().find(|m| m.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        let raw = include_str!("../../tests/fixtures/get_version-a1mini.json");
        serde_json::from_str(raw).expect("valid fixture json")
    }

    #[test]
    fn parses_firmware_from_the_ota_module() {
        let info = &fixture()["message"]["info"];
        let v = DeviceVersion::from_info(info);
        // The OTA module's sw_ver is the printer's firmware.
        assert_eq!(
            v.firmware.as_ref().map(ToString::to_string).as_deref(),
            Some("01.07.02.00")
        );
    }

    #[test]
    fn parses_all_modules_in_order() {
        let v = DeviceVersion::from_info(&fixture()["message"]["info"]);
        let names: Vec<&str> = v.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["ota", "esp32", "mc", "th", "ams_f1/0"]);
    }

    #[test]
    fn module_carries_hw_and_product_name() {
        let v = DeviceVersion::from_info(&fixture()["message"]["info"]);
        let ota = v.module("ota").unwrap();
        assert_eq!(ota.hw_ver.as_deref(), Some("OTA"));
        assert_eq!(ota.product_name.as_deref(), Some("Bambu Lab A1 mini"));
        // The AMS Lite module is identified by its product name.
        let ams = v.module("ams_f1/0").unwrap();
        assert_eq!(ams.hw_ver.as_deref(), Some("AMS_F102"));
        assert_eq!(ams.product_name.as_deref(), Some("AMS Lite"));
        // A module with an empty product_name drops it to None.
        assert_eq!(v.module("esp32").unwrap().product_name, None);
    }

    #[test]
    fn missing_ota_module_yields_no_firmware() {
        let info = json!({ "command": "get_version", "module": [
            { "name": "esp32", "sw_ver": "01.16.39.58" }
        ]});
        let v = DeviceVersion::from_info(&info);
        assert_eq!(v.firmware, None);
        assert_eq!(v.modules.len(), 1);
    }

    #[test]
    fn empty_or_absent_module_array_is_empty_inventory() {
        let v = DeviceVersion::from_info(&json!({ "command": "get_version" }));
        assert_eq!(v.firmware, None);
        assert!(v.modules.is_empty());
    }
}
