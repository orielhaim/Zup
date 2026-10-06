#![forbid(unsafe_code)]

//! The canonical Zup plugin ABI.
//!
//! A plugin is a WebAssembly component that exports one function, and this
//! crate is the definition of what that function is. It is deliberately the
//! smallest crate in the plugin system: WIT text, the digest that proves a
//! component was built against this exact contract, and the limits a guest is
//! held to. No engine, no linker, no host.
//!
//! # Why this crate exists
//!
//! A plugin author and the host that runs the plugin both need the WIT, and
//! neither of them needs the other. The author needs it to generate bindings;
//! the host needs it to validate a component and to prove that an ahead-of-time
//! artifact was produced against the same contract. Splitting those two
//! concerns is what lets a guest SDK depend on the contract without dragging
//! in a WebAssembly runtime, and lets a runtime validate a component without
//! owning the bindings generator.
//!
//! This crate also owns the WIT *file*. Both sides read it from here rather
//! than keeping a copy, so there is exactly one definition of the ABI and a
//! host cannot disagree with a guest about what it implements. A crate that
//! generates bindings from it reads this crate's build metadata for the path
//! rather than vendoring a second copy.

mod limits;

pub use limits::{
    AOT_FORMAT_VERSION, EPOCH_DEADLINE_TICKS, INVOCATION_DEADLINE_MILLIS, MAX_AOT_BYTES,
    MAX_FUEL_PER_CALL, MAX_HOST_CALLS, MAX_INSTANCES, MAX_MEMORY_BYTES, MAX_MEMORY_COUNT,
    MAX_MEMORY_PAGES, MAX_PLAN_OUTPUT_BYTES, MAX_PLAN_RESOURCES, MAX_TABLE_COUNT,
    MAX_TABLE_ELEMENTS, MAX_WASM_STACK_BYTES, PLUGIN_API_VERSION, WASM_PAGE_BYTES,
    WASMTIME_VERSION, WIT_PACKAGE_VERSION,
};

use sha2::{Digest, Sha256};

/// The WIT contract, verbatim.
///
/// The one copy of the plugin interface definition in this repository. Both the
/// guest SDK's bindings and the host's validation are generated from it, so a
/// change here is a change to the ABI rather than a refactor of one side of it.
pub const WIT_PACKAGE: &str = include_str!("../wit/zup-plugin.wit");

/// The world a Zup plugin exports.
pub const WORLD: &str = "plugin";

/// The interface a Zup plugin implements, as the host names it in diagnostics.
pub const PLANNER_EXPORT_NAME: &str = "zup:plugin/planner@1.0.0";

/// The one function a Zup plugin exports.
pub const PLAN_FUNCTION_NAME: &str = "plan";

/// The digest of [`WIT_PACKAGE`].
///
/// Written into every compiled plugin artifact and checked when one is loaded,
/// so a component built against a different contract is refused rather than
/// called with a signature its author never wrote.
///
/// Hashed as the contract reads rather than as it sits on disk. A checkout
/// rewrites line endings - git leaves a Windows working tree CRLF and a Unix one
/// LF - so hashing the bytes would make the digest identify the platform the
/// contract was checked out on rather than the contract, and an artifact built on
/// one would be refused by a host on the other for a difference no guest ever
/// observed. The WIT parser reads both the same way, so normalising to `\n`
/// hashes what the parser actually sees.
pub fn wit_package_digest() -> [u8; 32] {
    digest_canonical(WIT_PACKAGE)
}

/// Hash one WIT document's text, the way [`wit_package_digest`] hashes the
/// canonical one.
///
/// Split out so the line-ending rule is testable without a second checkout: this
/// is what makes the digest a property of the contract rather than of the
/// platform it was read on.
fn digest_canonical(wit: &str) -> [u8; 32] {
    let mut canonical = Vec::with_capacity(wit.len());
    let mut bytes = wit.as_bytes().iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'\r' {
            if bytes.peek() == Some(&b'\n') {
                bytes.next();
            }
            canonical.push(b'\n');
        } else {
            canonical.push(byte);
        }
    }
    Sha256::digest(&canonical).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest identifies the contract, and a checkout's line endings are not
    /// part of it.
    ///
    /// Without this, a Windows working tree and a Unix one produce two digests
    /// for one contract, and every artifact built on one is refused by a host on
    /// the other.
    #[test]
    fn the_digest_does_not_depend_on_line_endings() {
        let unix = "package zup:plugin@1.0.0;\nworld plugin {}\n";
        assert_eq!(
            digest_canonical(unix),
            digest_canonical("package zup:plugin@1.0.0;\r\nworld plugin {}\r\n"),
            "CRLF and LF describe the same contract"
        );
        assert_eq!(
            digest_canonical(unix),
            digest_canonical("package zup:plugin@1.0.0;\rworld plugin {}\r"),
            "and so does a lone carriage return"
        );
        assert_ne!(
            digest_canonical(unix),
            digest_canonical("package zup:plugin@1.0.0;\nworld plugin { }\n"),
            "while a change to the contract does move it"
        );
    }

    /// The shipped contract hashes to the value recorded in artifacts.
    #[test]
    fn the_digest_is_the_one_artifacts_record() {
        assert_eq!(
            wit_package_digest().to_vec(),
            digest_canonical(WIT_PACKAGE),
            "the published digest and the published contract agree"
        );
    }
}
