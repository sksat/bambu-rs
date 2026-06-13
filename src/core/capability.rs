//! Firmware capability / quirk registry.
//!
//! API behaviour varies by `(model, firmware)`. This registry resolves a set of
//! [`Capabilities`] from a model and firmware version, recording for each fact
//! *how well we know it* ([`Knowledge`] + [`Source`]). That keeps the clean-room
//! principle — **observed is truth, spec is reference, unobserved stays
//! unobserved** — encoded in the type system rather than collapsed into bare
//! `bool`s. The execution layer folds [`Knowledge`] to the safe side via
//! [`Capabilities::control_allowed`].

use crate::core::firmware::FirmwareVersion;
use crate::core::model::Model;

/// Where a capability fact comes from, ordered by trust:
/// `Assumed < Spec < Observed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// A conservative default guess, backed by neither spec nor device.
    Assumed,
    /// Documented in the protocol spec (OpenBambuAPI), not yet device-verified.
    Spec,
    /// Confirmed by direct observation of real hardware.
    Observed,
}

/// A capability fact together with how well we know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Knowledge<T> {
    Known { value: T, source: Source },
    Unknown,
}

impl<T> Knowledge<T> {
    pub fn known(value: T, source: Source) -> Self {
        Knowledge::Known { value, source }
    }

    pub fn is_known(&self) -> bool {
        matches!(self, Knowledge::Known { .. })
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            Knowledge::Known { value, .. } => Some(value),
            Knowledge::Unknown => None,
        }
    }

    pub fn source(&self) -> Option<Source> {
        match self {
            Knowledge::Known { source, .. } => Some(*source),
            Knowledge::Unknown => None,
        }
    }

    /// The value, but only if known from a source at least as trusted as `min`.
    /// This is how callers fold knowledge to the safe side.
    pub fn trusted(&self, min: Source) -> Option<&T> {
        match self {
            Knowledge::Known { value, source } if *source >= min => Some(value),
            _ => None,
        }
    }
}

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
    /// Model and firmware fall within known territory.
    Known,
    /// Model is known but firmware is newer than anything we have data for.
    FirmwareNewerThanKnown,
    /// Model is not in the registry.
    UnknownModel,
}

/// How much trust a caller requires before acting on a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityRequirement {
    /// Act even on assumed defaults.
    AllowAssumed,
    /// Act on spec-documented or observed facts (the usual default).
    RequireSpecOrObserved,
    /// Act only on device-observed facts (for the riskiest operations).
    RequireObserved,
}

impl CapabilityRequirement {
    fn min_source(self) -> Source {
        match self {
            CapabilityRequirement::AllowAssumed => Source::Assumed,
            CapabilityRequirement::RequireSpecOrObserved => Source::Spec,
            CapabilityRequirement::RequireObserved => Source::Observed,
        }
    }
}

/// Resolved capabilities for a `(model, firmware)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub model: Model,
    pub push_mode: Knowledge<PushMode>,
    pub camera_transport: Knowledge<CameraTransport>,
    pub developer_mode: Knowledge<DeveloperMode>,
    pub acs_policy: Knowledge<AcsPolicy>,
    pub registry_status: RegistryStatus,
}

impl Capabilities {
    /// Whether control commands (print start, heat, move, …) may be sent, given
    /// how much trust the caller requires.
    ///
    /// Folds [`Knowledge`] to the safe side: control is permitted only if we
    /// positively know — from a sufficiently trusted source — either that ACS
    /// does not gate control, or that Developer Mode is available. An unknown
    /// model or unknown firmware therefore refuses control.
    pub fn control_allowed(&self, req: CapabilityRequirement) -> bool {
        let min = req.min_source();
        if matches!(self.acs_policy.trusted(min), Some(AcsPolicy::NotRequired)) {
            return true;
        }
        matches!(
            self.developer_mode.trusted(min),
            Some(DeveloperMode::Available)
        )
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
    /// Highest firmware we have data/assumptions for; newer firmware is resolved
    /// best-effort but flagged [`RegistryStatus::FirmwareNewerThanKnown`].
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
            push_mode: Knowledge::Unknown,
            camera_transport: Knowledge::Unknown,
            developer_mode: Knowledge::Unknown,
            acs_policy: Knowledge::Unknown,
            registry_status: RegistryStatus::UnknownModel,
        };
    };

    let registry_status = if firmware > &profile.max_known_firmware {
        RegistryStatus::FirmwareNewerThanKnown
    } else {
        RegistryStatus::Known
    };

    // Model-class properties are documented (Spec) until device-observed.
    let push_mode = Knowledge::known(profile.push_mode, Source::Spec);
    let camera_transport = Knowledge::known(profile.camera_transport, Source::Spec);

    let (developer_mode, acs_policy) = match &profile.developer_mode_since {
        Some(threshold) => {
            let dev = if firmware >= threshold {
                DeveloperMode::Available
            } else {
                DeveloperMode::Unavailable
            };
            (
                Knowledge::known(dev, Source::Spec),
                Knowledge::known(AcsPolicy::Required, Source::Spec),
            )
        }
        None => (Knowledge::Unknown, Knowledge::Unknown),
    };

    Capabilities {
        model: model.clone(),
        push_mode,
        camera_transport,
        developer_mode,
        acs_policy,
        registry_status,
    }
}

/// The built-in registry of known models.
///
/// Values come from the OpenBambuAPI spec and prior research; every fact is
/// marked [`Source::Spec`] until confirmed on real hardware, at which point its
/// source should be upgraded to [`Source::Observed`]. Models are added as we
/// gather data.
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
    fn source_orders_by_trust() {
        assert!(Source::Assumed < Source::Spec);
        assert!(Source::Spec < Source::Observed);
    }

    #[test]
    fn trusted_requires_minimum_source() {
        let k = Knowledge::known(PushMode::DeltaOnly, Source::Spec);
        assert_eq!(k.trusted(Source::Assumed), Some(&PushMode::DeltaOnly));
        assert_eq!(k.trusted(Source::Spec), Some(&PushMode::DeltaOnly));
        assert_eq!(k.trusted(Source::Observed), None); // spec is not observed
        assert_eq!(
            Knowledge::<PushMode>::Unknown.trusted(Source::Assumed),
            None
        );
    }

    #[test]
    fn a1mini_is_delta_push_and_tcp_camera() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::A1Mini, &fw("01.05.00"));
        assert_eq!(caps.push_mode.value(), Some(&PushMode::DeltaOnly));
        assert_eq!(
            caps.camera_transport.value(),
            Some(&CameraTransport::JpegTcp6000)
        );
        assert_eq!(caps.registry_status, RegistryStatus::Known);
    }

    #[test]
    fn developer_mode_threshold_boundary() {
        let reg = default_registry();
        // Just before the threshold: unavailable.
        let before = resolve(&reg, &Model::A1Mini, &fw("01.04.99"));
        assert_eq!(
            before.developer_mode.value(),
            Some(&DeveloperMode::Unavailable)
        );
        // Exactly at and after the threshold (incl. trailing-zero forms): available.
        for v in ["01.05.00", "01.05", "01.05.00.00", "01.05.01"] {
            let caps = resolve(&reg, &Model::A1Mini, &fw(v));
            assert_eq!(
                caps.developer_mode.value(),
                Some(&DeveloperMode::Available),
                "firmware {v} should have Developer Mode"
            );
        }
    }

    #[test]
    fn control_is_refused_below_developer_mode_threshold() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::A1Mini, &fw("01.04.00"));
        assert!(!caps.control_allowed(CapabilityRequirement::RequireSpecOrObserved));
    }

    #[test]
    fn control_allowed_with_developer_mode_from_spec_but_not_observed() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::A1Mini, &fw("01.05.00"));
        // Spec-level knowledge is enough for the normal requirement...
        assert!(caps.control_allowed(CapabilityRequirement::RequireSpecOrObserved));
        // ...but the strictest requirement demands device-observed facts.
        assert!(!caps.control_allowed(CapabilityRequirement::RequireObserved));
    }

    #[test]
    fn unknown_model_resolves_to_unknown_and_refuses_control() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::Unknown("z9".into()), &fw("01.00.00"));
        assert_eq!(caps.registry_status, RegistryStatus::UnknownModel);
        assert!(!caps.push_mode.is_known());
        assert!(!caps.control_allowed(CapabilityRequirement::AllowAssumed));
    }

    #[test]
    fn known_model_in_registry_but_absent_profile_is_unknown() {
        // P1S is a known Model variant but has no profile yet.
        let reg = default_registry();
        let caps = resolve(&reg, &Model::P1S, &fw("01.00.00"));
        assert_eq!(caps.registry_status, RegistryStatus::UnknownModel);
    }

    #[test]
    fn firmware_newer_than_known_is_flagged_but_still_resolves() {
        let reg = default_registry();
        let caps = resolve(&reg, &Model::A1Mini, &fw("01.99.00"));
        assert_eq!(caps.registry_status, RegistryStatus::FirmwareNewerThanKnown);
        // Still resolved best-effort: above threshold => Developer Mode available.
        assert_eq!(caps.developer_mode.value(), Some(&DeveloperMode::Available));
    }
}
