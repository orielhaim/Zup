use crate::{Capabilities, PRESET_PROTOCOL_VERSION};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostOffers {
    pub protocol: u32,
    pub capabilities: Capabilities,
}

impl HostOffers {
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            protocol: PRESET_PROTOCOL_VERSION,
            capabilities,
        }
    }

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

    pub fn check(
        &self,
        required_protocol: u32,
        required: &Capabilities,
    ) -> Result<(), Incompatible> {
        self.incompatibility(required_protocol, required)
            .map_or(Ok(()), Err)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Incompatible {
    #[error("the preset speaks preset protocol {found}; this host speaks {expected}")]
    Protocol { expected: u32, found: u32 },
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

    #[test]
    fn a_preset_that_needs_nothing_is_always_presentable() {
        let host = offers(&[Capability::Components]);
        host.check(PRESET_PROTOCOL_VERSION, &Capabilities::default())
            .expect("an empty requirement is met");
    }

    #[test]
    fn a_provided_capability_is_not_a_missing_one() {
        let host = offers(&[Capability::Components, Capability::PlanPreview]);
        host.check(
            PRESET_PROTOCOL_VERSION,
            &Capabilities::new([Capability::Components]),
        )
        .expect("the host provides components");
    }

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
            error.to_string().contains("preset protocol"),
            "the message names the axis: {error}"
        );
    }

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
