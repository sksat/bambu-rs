//! Firmware capability / quirk registry.
//!
//! API behaviour varies by `(model, firmware)`. [`resolve`] looks a model and
//! firmware version up in a [`CapabilityRegistry`] and returns the known
//! [`Capabilities`]; unknown facts are simply `None`.
//!
//! The **clean-room** principle — observed is truth, we never claim to know what
//! we haven't — lives in the *registration rules*, not in the value types: we
//! only put a model in the registry once we understand it, only map a device
//! code we've actually seen (see [`Model::from_device_code`](crate::core::model::Model::from_device_code)),
//! and refuse control whenever the `(model, firmware)` falls outside what the
//! registry covers (see [`Capabilities::control_permission`]).

use crate::core::firmware::FirmwareVersion;
use crate::core::model::Model;

/// Whether the printer pushes its full state each time (X1 class) or only deltas
/// that the client must cache and merge (P1/A1 class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushMode {
    Full,
    DeltaOnly,
}

/// How the camera is reached, which differs sharply by model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraTransport {
    /// RTSP over TLS on port 322 (X1 / X1E / H2D).
    Rtsp322,
    /// Proprietary JPEG stream over TCP port 6000 (P1 / A1).
    JpegTcp6000,
    /// No LAN camera access.
    None,
}

/// Whether the printer's "Developer Mode" (LAN-only control) is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeveloperMode {
    Available,
    Unavailable,
}

/// Whether the Authorization Control System gates third-party control commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcsPolicy {
    /// Control requires LAN-only + Developer Mode.
    Required,
    /// Control is not gated.
    NotRequired,
}

/// How well the registry matched a `(model, firmware)` query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryStatus {
    /// Model and firmware fall within known, supported territory.
    Supported,
    /// Model is known but firmware is newer than anything we have data for.
    FirmwareNewerThanKnown,
    /// Model is not in the registry.
    UnknownModel,
}

/// Permission to send control commands (print start, heat, move, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPermit {
    /// Control is not gated by ACS.
    Allowed,
    /// Allowed, but the printer must be in LAN-only + Developer Mode.
    RequiresDeveloperMode,
}

/// Why control was refused — distinct reasons so the CLI can map them to exit
/// codes and actionable messages, and so a caller can choose to override a
/// specific one (e.g. allow an unknown firmware) at its own risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRefusal {
    /// We don't recognise this model at all.
    UnknownModel,
    /// Firmware is newer than the registry knows; ACS behaviour may have
    /// changed, so we refuse rather than risk an uncatalogued change.
    FirmwareNewerThanKnown,
    /// Developer Mode is required but not available on this firmware.
    DeveloperModeUnavailable,
    /// We don't know this model's control/ACS boundary.
    UnknownControlBoundary,
}

/// Resolved capabilities for a `(model, firmware)`. Unknown facts are `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub model: Model,
    pub push_mode: Option<PushMode>,
    pub camera_transport: Option<CameraTransport>,
    pub developer_mode: Option<DeveloperMode>,
    pub acs_policy: Option<AcsPolicy>,
    pub registry_status: RegistryStatus,
}

impl Capabilities {
    /// Whether — and under what condition — control commands may be sent.
    ///
    /// Safety is decided here, directly from registry-match quality and the
    /// control boundary, rather than from per-fact provenance:
    /// an unknown model or firmware-newer-than-known refuses control outright.
    pub fn control_permission(&self) -> Result<ControlPermit, ControlRefusal> {
        match self.registry_status {
            RegistryStatus::UnknownModel => return Err(ControlRefusal::UnknownModel),
            RegistryStatus::FirmwareNewerThanKnown => {
                return Err(ControlRefusal::FirmwareNewerThanKnown);
            }
            RegistryStatus::Supported => {}
        }
        match (self.acs_policy, self.developer_mode) {
            (Some(AcsPolicy::NotRequired), _) => Ok(ControlPermit::Allowed),
            (_, Some(DeveloperMode::Available)) => Ok(ControlPermit::RequiresDeveloperMode),
            (_, Some(DeveloperMode::Unavailable)) => Err(ControlRefusal::DeveloperModeUnavailable),
            _ => Err(ControlRefusal::UnknownControlBoundary),
        }
    }

    /// Convenience: whether control is permitted at all (see
    /// [`Capabilities::control_permission`] for the reason).
    pub fn control_allowed(&self) -> bool {
        self.control_permission().is_ok()
    }
}

/// Per-model data the registry holds.
struct ModelProfile {
    model: Model,
    push_mode: PushMode,
    camera_transport: CameraTransport,
    /// Firmware at/after which Developer Mode exists (and ACS gates control).
    /// `None` means we don't have a documented threshold for this model.
    developer_mode_since: Option<FirmwareVersion>,
    /// Highest firmware we have data for; newer firmware resolves best-effort
    /// but is flagged [`RegistryStatus::FirmwareNewerThanKnown`] and refused
    /// control.
    max_known_firmware: FirmwareVersion,
}

/// A set of known model profiles. Construct via [`default_registry`] or build a
/// custom one in tests — there is intentionally no global singleton.
pub struct CapabilityRegistry {
    profiles: Vec<ModelProfile>,
}

impl CapabilityRegistry {
    fn profile(&self, model: &Model) -> Option<&ModelProfile> {
        self.profiles.iter().find(|p| &p.model == model)
    }
}

/// Resolve capabilities for a `(model, firmware)`. A pure function over an
/// explicit registry — no globals, no I/O — so it is trivially testable.
pub fn resolve(
    registry: &CapabilityRegistry,
    model: &Model,
    firmware: &FirmwareVersion,
) -> Capabilities {
    let Some(profile) = registry.profile(model) else {
        return Capabilities {
            model: model.clone(),
            push_mode: None,
            camera_transport: None,
            developer_mode: None,
            acs_policy: None,
            registry_status: RegistryStatus::UnknownModel,
        };
    };

    let registry_status = if firmware > &profile.max_known_firmware {
        RegistryStatus::FirmwareNewerThanKnown
    } else {
        RegistryStatus::Supported
    };

    let (developer_mode, acs_policy) = match &profile.developer_mode_since {
        Some(threshold) => {
            let dev = if firmware >= threshold {
                DeveloperMode::Available
            } else {
                DeveloperMode::Unavailable
            };
            (Some(dev), Some(AcsPolicy::Required))
        }
        None => (None, None),
    };

    Capabilities {
        model: model.clone(),
        push_mode: Some(profile.push_mode),
        camera_transport: Some(profile.camera_transport),
        developer_mode,
        acs_policy,
        registry_status,
    }
}

/// The built-in registry of known models.
///
/// Values come from the OpenBambuAPI spec and prior research; the A1 mini entry
/// is the one we have actually observed. Models are added as we gather data.
pub fn default_registry() -> CapabilityRegistry {
    let fw = |s: &str| FirmwareVersion::parse(s).expect("valid firmware literal");
    CapabilityRegistry {
        profiles: vec![ModelProfile {
            model: Model::A1Mini,
            push_mode: PushMode::DeltaOnly,
            camera_transport: CameraTransport::JpegTcp6000,
            developer_mode_since: Some(fw("01.05.00")),
            max_known_firmware: fw("01.06.00"),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fw(s: &str) -> FirmwareVersion {
        FirmwareVersion::parse(s).unwrap()
    }

    #[test]
    fn a1mini_is_delta_push_and_tcp_camera() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::A1Mini, &fw("01.05.00"));
        assert_eq!(caps.push_mode, Some(PushMode::DeltaOnly));
        assert_eq!(caps.camera_transport, Some(CameraTransport::JpegTcp6000));
        assert_eq!(caps.registry_status, RegistryStatus::Supported);
    }

    #[test]
    fn developer_mode_threshold_boundary() {
        let reg = default_registry();
        let before = resolve(&reg, &Model::A1Mini, &fw("01.04.99"));
        assert_eq!(before.developer_mode, Some(DeveloperMode::Unavailable));
        for v in ["01.05.00", "01.05", "01.05.00.00", "01.05.01"] {
            let caps = resolve(&reg, &Model::A1Mini, &fw(v));
            assert_eq!(
                caps.developer_mode,
                Some(DeveloperMode::Available),
                "firmware {v} should have Developer Mode"
            );
        }
    }

    #[test]
    fn control_refused_below_developer_mode_threshold() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::A1Mini, &fw("01.04.00"));
        assert_eq!(
            caps.control_permission(),
            Err(ControlRefusal::DeveloperModeUnavailable)
        );
        assert!(!caps.control_allowed());
    }

    #[test]
    fn control_allowed_via_developer_mode_when_supported() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::A1Mini, &fw("01.05.00"));
        assert_eq!(
            caps.control_permission(),
            Ok(ControlPermit::RequiresDeveloperMode)
        );
        assert!(caps.control_allowed());
    }

    #[test]
    fn unknown_model_resolves_to_unknown_and_refuses_control() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::Unknown("z9".into()), &fw("01.00.00"));
        assert_eq!(caps.registry_status, RegistryStatus::UnknownModel);
        assert_eq!(caps.push_mode, None);
        assert_eq!(caps.control_permission(), Err(ControlRefusal::UnknownModel));
    }

    #[test]
    fn known_model_variant_without_a_profile_is_unknown() {
        // P1S is a known Model variant but has no profile yet.
        let reg = default_registry();
        let caps = resolve(&reg, &Model::P1S, &fw("01.00.00"));
        assert_eq!(caps.registry_status, RegistryStatus::UnknownModel);
    }

    #[test]
    fn firmware_newer_than_known_refuses_control_directly() {
        // The P1 safety boundary, expressed directly via registry_status rather
        // than by downgrading per-fact provenance.
        let reg = default_registry();
        let caps = resolve(&reg, &Model::A1Mini, &fw("01.99.00"));
        assert_eq!(caps.registry_status, RegistryStatus::FirmwareNewerThanKnown);
        assert_eq!(
            caps.control_permission(),
            Err(ControlRefusal::FirmwareNewerThanKnown)
        );
        // The descriptive facts are still resolved best-effort.
        assert_eq!(caps.push_mode, Some(PushMode::DeltaOnly));
        assert_eq!(caps.developer_mode, Some(DeveloperMode::Available));
    }
}
