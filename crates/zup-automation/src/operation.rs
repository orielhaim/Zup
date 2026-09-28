//! The operation names the developer CLI reports.
//!
//! These are zup's semantics, not an orchestrator's. A pipeline that calls its own
//! steps "compose" and "release" is describing itself; the operation that ran was
//! `publish.stage` and `publish.github`, and a protocol that reported the orchestrator's
//! word would be a contract every other consumer would have to translate. So the
//! Action maps its phases onto these, and these never mention a phase.
//!
//! The list is a set of *constants*, not an enum. Adding an operation is additive: an
//! older consumer reading `{"operation": "toolchain.warm"}` is told to ignore what it
//! does not recognise, and a closed enum would have made that a wire break. See
//! [`Identifier`] for the grammar and [`crate::version`] for why a minor may add one.

use crate::identifier::Identifier;

/// Build the declared distribution artifacts.
pub const OPERATION_BUILD: &str = "build";
/// Validate a project and the files it would ship.
pub const OPERATION_CHECK: &str = "check";
/// Report whether this project is ready to build.
pub const OPERATION_DOCTOR: &str = "doctor";
/// Show what installing this project would do.
pub const OPERATION_PLAN: &str = "plan";
/// Stage the web tree a static origin serves and a TUF repository signs.
pub const OPERATION_PUBLISH_STAGE: &str = "publish.stage";
/// Publish a staged release to a provider.
pub const OPERATION_PUBLISH_GITHUB: &str = "publish.github";
/// Write the list of files that require a signature.
pub const OPERATION_SIGN_PREPARE: &str = "sign.prepare";
/// Verify every signature and finalize the release description.
pub const OPERATION_SIGN_VERIFY: &str = "sign.verify";
/// Copy a zup release's components into this machine's toolchain cache.
pub const OPERATION_TOOLCHAIN_INSTALL: &str = "toolchain.install";
/// Report what a build here would resolve, and from where.
pub const OPERATION_TOOLCHAIN_STATUS: &str = "toolchain.status";
/// Remove cached toolchains this zup cannot use.
pub const OPERATION_TOOLCHAIN_CLEAN: &str = "toolchain.clean";
/// Describe a built artifact.
pub const OPERATION_ARTIFACT_INSPECT: &str = "artifact.inspect";

/// Every operation this build of zup can name.
///
/// The full set, for the checks that are about the set rather than about one name:
/// `--help` text, a compatibility gate, a consumer that wants to know what it might
/// meet. Not a closed vocabulary on the wire — [`Operation`] is a string, and a newer
/// zup may send a name that is not in here.
pub const ALL_OPERATIONS: &[&str] = &[
    OPERATION_BUILD,
    OPERATION_CHECK,
    OPERATION_DOCTOR,
    OPERATION_PLAN,
    OPERATION_PUBLISH_STAGE,
    OPERATION_PUBLISH_GITHUB,
    OPERATION_SIGN_PREPARE,
    OPERATION_SIGN_VERIFY,
    OPERATION_TOOLCHAIN_INSTALL,
    OPERATION_TOOLCHAIN_STATUS,
    OPERATION_TOOLCHAIN_CLEAN,
    OPERATION_ARTIFACT_INSPECT,
];

/// The operation one developer CLI invocation performed.
///
/// A validated identifier rather than an enum, and a value rather than a flag, because
/// an integration has to be able to branch on the operation it asked for without the
/// protocol growing a case for every verb the CLI gains.
pub type Operation = Identifier;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every constant obeys the wire grammar. A constant that did not would be a
    /// document that cannot be read back, and the only place that would be noticed is
    /// a consumer's log.
    #[test]
    fn every_operation_name_is_a_wire_identifier() {
        for name in ALL_OPERATIONS {
            assert!(
                Operation::parse(name).is_ok(),
                "`{name}` is not an identifier"
            );
        }
    }

    /// The operations that take a compound form take it with a dot, and the
    /// orchestrator's vocabulary is deliberately absent: nothing here is called
    /// `compose`, `release`, `setup` or `attest`, because none of those is something
    /// zup does.
    #[test]
    fn the_names_describe_zup_and_not_an_orchestrator() {
        for name in [OPERATION_PUBLISH_STAGE, OPERATION_PUBLISH_GITHUB] {
            let operation = Operation::parse(name).expect("an operation");
            assert_eq!(operation.segments().next(), Some("publish"));
        }
        for forbidden in ["compose", "release", "setup", "attest", "finalize"] {
            assert!(
                !ALL_OPERATIONS.contains(&forbidden),
                "`{forbidden}` is an orchestrator's word, not a zup operation"
            );
        }
    }
}
