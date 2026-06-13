//! Firmware capability / quirk registry.
//!
//! API and hardware behaviour vary by `(model, firmware)`. [`resolve`] looks a
//! model and firmware up in a [`CapabilityRegistry`] and returns the known
//! [`Capabilities`]; unknown facts are simply `None`.
//!
//! ## Two separated concerns
//!
//! - **Descriptive** capabilities (push mode, camera transport, hardware
//!   features) describe how to *talk to and interpret* a printer. We register
//!   these even for models we've only documented, so discovery/status work.
//! - The **control boundary** (may we send control commands, and under what
//!   firmware gating) is a *safety* decision. It is granted only for models we
//!   are confident enough about; an unknown model, a firmware newer than we
//!   know, or a model without a confirmed control boundary all refuse control
//!   (see [`Capabilities::control_permission`]).
//!
//! Provenance ([`EvidenceGrade`]) travels per *model profile* as audit
//! metadata; it is deliberately **not** consulted by `control_permission`
//! (per-fact provenance was rejected as over-engineering — safety lives in the
//! control boundary, not in a confidence tag).

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

/// How a model's `chamber_temper` report field should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChamberTemperature {
    /// A real chamber sensor (enclosed X1 / X1E / H2D).
    RealSensor,
    /// The field is emitted but is **not** a real sensor (A1 / P1) — a typed
    /// status must not surface it as a temperature.
    ReportedSynthetic,
    /// Not present at all.
    Unsupported,
}

/// Hardware features that change how reports are interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardwareFeatures {
    /// Micro-LiDAR (X1 / X1C / X1E) — gates `0x0C` (XCAM) HMS codes and the
    /// lidar calibration bit.
    pub lidar: bool,
    pub chamber_temperature: ChamberTemperature,
    pub aux_fan: bool,
    pub chamber_fan: bool,
}

/// Whether the printer's "Developer Mode" (LAN-only control) is available at the
/// queried firmware.
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

/// How well-evidenced a model profile is. Audit metadata only — **never** used
/// to decide control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceGrade {
    /// Confirmed on our own hardware.
    Observed,
    /// Bambu's own slicer machine list (vendor-canonical).
    VendorSpec,
    /// OpenBambuAPI protocol documentation.
    DocSpec,
    /// Extrapolated; never observed on the wire.
    Inferred,
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

/// Why control was refused — distinct reasons so a caller (CLI) can map them to
/// exit codes and actionable messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRefusal {
    /// We don't recognise this model at all.
    UnknownModel,
    /// Firmware is newer than the registry knows; ACS behaviour may have
    /// changed, so we refuse rather than risk an uncatalogued change.
    FirmwareNewerThanKnown,
    /// Developer Mode is required but not available on this firmware.
    DeveloperModeUnavailable,
    /// We don't have a confirmed control boundary for this model.
    UnknownControlBoundary,
}

/// Resolved capabilities for a `(model, firmware)`. Unknown facts are `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub model: Model,
    pub push_mode: Option<PushMode>,
    pub camera_transport: Option<CameraTransport>,
    pub hardware: Option<HardwareFeatures>,
    pub developer_mode: Option<DeveloperMode>,
    pub acs_policy: Option<AcsPolicy>,
    pub evidence: Option<EvidenceGrade>,
    pub registry_status: RegistryStatus,
}

impl Capabilities {
    /// Whether — and under what condition — control commands may be sent.
    ///
    /// Safety is decided from registry-match quality and the control boundary,
    /// never from [`EvidenceGrade`]: an unknown model, firmware newer than
    /// known, or a model without a confirmed control boundary all refuse.
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
            (Some(AcsPolicy::Required), Some(DeveloperMode::Available)) => {
                Ok(ControlPermit::RequiresDeveloperMode)
            }
            (Some(AcsPolicy::Required), Some(DeveloperMode::Unavailable)) => {
                Err(ControlRefusal::DeveloperModeUnavailable)
            }
            // No confirmed control boundary (developer_mode/acs are None).
            _ => Err(ControlRefusal::UnknownControlBoundary),
        }
    }

    /// Convenience: whether control is permitted at all.
    pub fn control_allowed(&self) -> bool {
        self.control_permission().is_ok()
    }
}

/// The confirmed control gating for a model. Present only for models we are
/// confident enough about to permit control.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ControlBoundary {
    /// Firmware at/after which Developer Mode exists (and ACS gates control).
    developer_mode_since: FirmwareVersion,
    acs_policy: AcsPolicy,
}

/// Per-model data the registry holds.
struct ModelProfile {
    model: Model,
    // Descriptive (registered even for less-certain models):
    push_mode: PushMode,
    camera_transport: CameraTransport,
    hardware: HardwareFeatures,
    evidence: EvidenceGrade,
    /// Highest firmware we understand; newer firmware refuses control.
    max_known_firmware: FirmwareVersion,
    // Safety:
    /// `None` => no confirmed control boundary => control is refused.
    control_boundary: Option<ControlBoundary>,
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
            hardware: None,
            developer_mode: None,
            acs_policy: None,
            evidence: None,
            registry_status: RegistryStatus::UnknownModel,
        };
    };

    let registry_status = if firmware > &profile.max_known_firmware {
        RegistryStatus::FirmwareNewerThanKnown
    } else {
        RegistryStatus::Supported
    };

    let (developer_mode, acs_policy) = match &profile.control_boundary {
        Some(cb) => {
            let dev = if firmware >= &cb.developer_mode_since {
                DeveloperMode::Available
            } else {
                DeveloperMode::Unavailable
            };
            (Some(dev), Some(cb.acs_policy))
        }
        None => (None, None),
    };

    Capabilities {
        model: model.clone(),
        push_mode: Some(profile.push_mode),
        camera_transport: Some(profile.camera_transport),
        hardware: Some(profile.hardware),
        developer_mode,
        acs_policy,
        evidence: Some(profile.evidence),
        registry_status,
    }
}

/// The built-in registry of known models.
///
/// The A1 mini entry is **hardware-observed** (model code, firmware
/// `01.07.02.00` and AMS-Lite confirmed on a real unit). Other entries are
/// vendor-canonical (`EvidenceGrade::VendorSpec`) or, for the newest gear,
/// inferred — and the inferred ones deliberately carry **no control boundary**.
pub fn default_registry() -> CapabilityRegistry {
    let fw = |s: &str| FirmwareVersion::parse(s).expect("valid firmware literal");

    let a1_family_hw = HardwareFeatures {
        lidar: false,
        chamber_temperature: ChamberTemperature::ReportedSynthetic, // emitted but inert
        aux_fan: false,
        chamber_fan: false,
    };
    let p1_hw = HardwareFeatures {
        lidar: false,
        chamber_temperature: ChamberTemperature::ReportedSynthetic,
        aux_fan: true,
        chamber_fan: true,
    };
    let x1_hw = HardwareFeatures {
        lidar: true,
        chamber_temperature: ChamberTemperature::RealSensor,
        aux_fan: true,
        chamber_fan: true,
    };

    let a1_boundary = || {
        Some(ControlBoundary {
            developer_mode_since: fw("01.05.00"),
            acs_policy: AcsPolicy::Required,
        })
    };
    let p1_boundary = || {
        Some(ControlBoundary {
            developer_mode_since: fw("01.08.02"),
            acs_policy: AcsPolicy::Required,
        })
    };
    let x1_boundary = || {
        Some(ControlBoundary {
            developer_mode_since: fw("01.08.03"),
            acs_policy: AcsPolicy::Required,
        })
    };

    CapabilityRegistry {
        profiles: vec![
            // OBSERVED: our real A1 mini (firmware 01.07.02.00, hw_ver AP05, AMS Lite).
            ModelProfile {
                model: Model::A1Mini,
                push_mode: PushMode::DeltaOnly,
                camera_transport: CameraTransport::JpegTcp6000,
                hardware: a1_family_hw,
                evidence: EvidenceGrade::Observed,
                max_known_firmware: fw("01.07.02"),
                control_boundary: a1_boundary(),
            },
            // VendorSpec: A1 (full) — same family as the observed A1 mini.
            ModelProfile {
                model: Model::A1,
                push_mode: PushMode::DeltaOnly,
                camera_transport: CameraTransport::JpegTcp6000,
                hardware: a1_family_hw,
                evidence: EvidenceGrade::VendorSpec,
                max_known_firmware: fw("01.07.02"),
                control_boundary: a1_boundary(),
            },
            ModelProfile {
                model: Model::P1P,
                push_mode: PushMode::DeltaOnly,
                camera_transport: CameraTransport::JpegTcp6000,
                hardware: p1_hw,
                evidence: EvidenceGrade::VendorSpec,
                max_known_firmware: fw("01.08.04"),
                control_boundary: p1_boundary(),
            },
            ModelProfile {
                model: Model::P1S,
                push_mode: PushMode::DeltaOnly,
                camera_transport: CameraTransport::JpegTcp6000,
                hardware: p1_hw,
                evidence: EvidenceGrade::VendorSpec,
                max_known_firmware: fw("01.08.04"),
                control_boundary: p1_boundary(),
            },
            ModelProfile {
                model: Model::X1Carbon,
                push_mode: PushMode::Full,
                camera_transport: CameraTransport::Rtsp322,
                hardware: x1_hw,
                evidence: EvidenceGrade::VendorSpec,
                max_known_firmware: fw("01.08.05"),
                control_boundary: x1_boundary(),
            },
            ModelProfile {
                model: Model::X1E,
                push_mode: PushMode::Full,
                camera_transport: CameraTransport::Rtsp322,
                hardware: x1_hw,
                evidence: EvidenceGrade::VendorSpec,
                max_known_firmware: fw("01.08.05"),
                control_boundary: x1_boundary(),
            },
            // INFERRED: H2D SSDP code never observed, push mode inferred — keep
            // descriptive info but grant NO control boundary (control refused).
            ModelProfile {
                model: Model::H2D,
                push_mode: PushMode::Full,
                camera_transport: CameraTransport::Rtsp322,
                hardware: HardwareFeatures {
                    lidar: false,
                    chamber_temperature: ChamberTemperature::RealSensor,
                    aux_fan: true,
                    chamber_fan: true,
                },
                evidence: EvidenceGrade::Inferred,
                max_known_firmware: fw("01.02.00"),
                control_boundary: None,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fw(s: &str) -> FirmwareVersion {
        FirmwareVersion::parse(s).unwrap()
    }

    #[test]
    fn observed_a1mini_descriptive_facts() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::A1Mini, &fw("01.07.02"));
        assert_eq!(caps.push_mode, Some(PushMode::DeltaOnly));
        assert_eq!(caps.camera_transport, Some(CameraTransport::JpegTcp6000));
        assert_eq!(caps.evidence, Some(EvidenceGrade::Observed));
        assert_eq!(caps.registry_status, RegistryStatus::Supported);
        let hw = caps.hardware.unwrap();
        assert!(!hw.lidar);
        assert_eq!(
            hw.chamber_temperature,
            ChamberTemperature::ReportedSynthetic
        );
        assert!(!hw.aux_fan && !hw.chamber_fan);
    }

    #[test]
    fn a1mini_real_firmware_is_supported_not_newer_than_known() {
        // Regression: max_known_firmware must cover the real device (01.07.02.00),
        // otherwise control would be wrongly refused as FirmwareNewerThanKnown.
        let reg = default_registry();
        let caps = resolve(&reg, &Model::A1Mini, &fw("01.07.02.00"));
        assert_eq!(caps.registry_status, RegistryStatus::Supported);
        assert_eq!(
            caps.control_permission(),
            Ok(ControlPermit::RequiresDeveloperMode)
        );
    }

    #[test]
    fn developer_mode_threshold_and_control_below_it() {
        let reg = default_registry();
        let below = resolve(&reg, &Model::A1Mini, &fw("01.04.99"));
        assert_eq!(below.developer_mode, Some(DeveloperMode::Unavailable));
        assert_eq!(
            below.control_permission(),
            Err(ControlRefusal::DeveloperModeUnavailable)
        );
    }

    #[test]
    fn unknown_model_refuses_control() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::Unknown("z9".into()), &fw("01.00.00"));
        assert_eq!(caps.registry_status, RegistryStatus::UnknownModel);
        assert_eq!(caps.push_mode, None);
        assert_eq!(caps.control_permission(), Err(ControlRefusal::UnknownModel));
    }

    #[test]
    fn firmware_newer_than_known_refuses_control() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::A1Mini, &fw("01.99.00"));
        assert_eq!(caps.registry_status, RegistryStatus::FirmwareNewerThanKnown);
        assert_eq!(
            caps.control_permission(),
            Err(ControlRefusal::FirmwareNewerThanKnown)
        );
        // Descriptive facts still resolve.
        assert_eq!(caps.push_mode, Some(PushMode::DeltaOnly));
    }

    #[test]
    fn x1_carbon_is_full_push_rtsp_and_has_lidar() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::X1Carbon, &fw("01.08.03"));
        assert_eq!(caps.push_mode, Some(PushMode::Full));
        assert_eq!(caps.camera_transport, Some(CameraTransport::Rtsp322));
        let hw = caps.hardware.unwrap();
        assert!(hw.lidar);
        assert_eq!(hw.chamber_temperature, ChamberTemperature::RealSensor);
        assert_eq!(
            caps.control_permission(),
            Ok(ControlPermit::RequiresDeveloperMode)
        );
    }

    #[test]
    fn inferred_h2d_keeps_descriptive_info_but_refuses_control() {
        // The crux of descriptive-vs-control separation: H2D facts are present
        // for discovery, but with no confirmed control boundary control is
        // refused even though the model and firmware are "in range".
        let reg = default_registry();
        let caps = resolve(&reg, &Model::H2D, &fw("01.01.05"));
        assert_eq!(caps.registry_status, RegistryStatus::Supported);
        assert_eq!(caps.push_mode, Some(PushMode::Full)); // descriptive present
        assert_eq!(caps.evidence, Some(EvidenceGrade::Inferred));
        assert_eq!(caps.developer_mode, None);
        assert_eq!(
            caps.control_permission(),
            Err(ControlRefusal::UnknownControlBoundary)
        );
    }
}
