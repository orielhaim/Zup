//! Whether this host can present this preset.
//!
//! Two axes, and both have to pass. The wire version is the coarse one: a frame
//! whose shape a peer cannot follow is not something to negotiate, so a preset
//! built against a different wire version is refused. Capabilities are the fine
//! one, and they are what lets functionality evolve independently of everything
//! else - a host that gains a capability can present presets that need it without
//! either side moving a version number.
//!
//! This is one rule stated once, because a build and a host both have to apply
//! it and a rule that exists twice is a rule that will be applied twice
//! differently.

use crate::{PRESET_PROTOCOL_VERSION, Capabilities};

/// What a host offers a preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostOffers {
    /// The wire version this host speaks.
    pub protocol: u32,
    /// The capabilities this host provides.
    pub capabilities: Capabilities,
}

impl HostOffers {
    /// What this host provides, at the current wire version.
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            protocol: PRESET_PROTOCOL_VERSION,
            capabilities,
        }
    }

    /// What a preset requires that this host does not provide.
    ///
    /// `None` means it can be presented. The two reasons stay distinct because
    /// they are different mistakes: one is a preset for a different generation
    /// of this protocol, the other is a preset asking for something this
    /// application cannot do.
    pub fn incompatibility(
        &self,
        required_protocol: u32,
        required: &Capabilities,
    ) -> Option<Incompatible> {
        if required_protocol != self.protocol {
            return Some(Incompatible::Protocol {
                expected: self.protocol,
                found: required_protocol,
            });
        }
        let missing = self.capabilities.missing(required);
        if missing.is_empty() {
            None
        } else {
            Some(Incompatible::Capabilities {
                missing: missing.join(", "),
            })
        }
    }

    /// Whether this host can present a preset with these requirements.
    pub fn check(
        &self,
        required_protocol: u32,
        required: &Capabilities,
    ) -> Result<(), Incompatible> {
        self.incompatibility(required_protocol, required)
            .map_or(Ok(()), Err)
    }
}

/// Why a host cannot present a preset.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Incompatible {
    /// The preset was built against a different wire version.
    #[error("the preset speaks UI protocol {found}; this host speaks {expected}")]
    Protocol { expected: u32, found: u32 },
    /// The preset needs capabilities this host does not provide.
    #[error("the preset needs {missing}, which this host does not provide")]
    Capabilities { missing: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Capability;

    fn offers(capabilities: &[Capability]) -> HostOffers {
        HostOffers::new(Capabilities::new(capabilities.iter().copied()))
    }

    /// A preset that needs nothing is presentable by any host at this version.
    #[test]
    fn a_preset_that_needs_nothing_is_always_presentable() {
        let host = offers(&[Capability::Components]);
        host.check(PRESET_PROTOCOL_VERSION, &Capabilities::default())
            .expect("an empty requirement is met");
    }

    /// Capabilities this host has are not a requirement it fails.
    #[test]
    fn a_provided_capability_is_not_a_missing_one() {
        let host = offers(&[Capability::Components, Capability::PlanPreview]);
        host.check(
            PRESET_PROTOCOL_VERSION,
            &Capabilities::new([Capability::Components]),
        )
        .expect("the host provides components");
    }

    /// A capability nobody provides is named, and named as more than a boolean.
    #[test]
    fn a_missing_capability_is_named() {
        let host = offers(&[Capability::Components]);
        let required = Capabilities::new([Capability::Components, Capability::Updates]);
        let error = host
            .check(PRESET_PROTOCOL_VERSION, &required)
            .expect_err("the host provides no updates");
        assert_eq!(
            error,
            Incompatible::Capabilities {
                missing: "updates".to_owned()
            }
        );
    }

    /// A different wire version is a different problem from a missing
    /// capability, and is reported as one: neither is fixable by adding a
    /// capability, and telling someone to add one when the real problem is a
    /// protocol generation would send them looking in the wrong place.
    #[test]
    fn a_different_wire_version_is_its_own_refusal() {
        let host = offers(&[]);
        let error = host
            .check(PRESET_PROTOCOL_VERSION + 1, &Capabilities::default())
            .expect_err("a preset from another protocol generation");
        assert_eq!(
            error,
            Incompatible::Protocol {
                expected: PRESET_PROTOCOL_VERSION,
                found: PRESET_PROTOCOL_VERSION + 1,
            }
        );
        assert!(
            error.to_string().contains("UI protocol"),
            "the message names the axis: {error}"
        );
    }

    /// The wire version is checked first, so a preset that is wrong in both ways
    /// is told about the generation rather than about a capability that would
    /// not have mattered.
    #[test]
    fn the_wire_version_is_reported_before_a_capability() {
        let host = offers(&[]);
        let required = Capabilities::new([Capability::Components]);
        let error = host
            .check(PRESET_PROTOCOL_VERSION + 1, &required)
            .expect_err("wrong in both ways");
        assert!(matches!(error, Incompatible::Protocol { .. }), "{error}");
    }
}
