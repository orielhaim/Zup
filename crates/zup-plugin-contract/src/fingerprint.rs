//! What an ahead-of-time plugin artifact records, and how it is checked.
//!
//! A precompiled component is only loadable by the exact engine that produced
//! it, so an artifact carries enough to prove it was: the Wasmtime version, the
//! layout version, the ABI version, the digest of the WIT, and a fingerprint of
//! the engine configuration. Every one of those is compared before a
//! deserializer is handed a byte of the artifact.

use std::fmt::{self, Write as _};

use sha2::{Digest, Sha256};

use crate::config::{
    EPOCH_DEADLINE_TICKS, INVOCATION_DEADLINE_MILLIS, MAX_FUEL_PER_CALL, MAX_HOST_CALLS,
    MAX_INSTANCES, MAX_MEMORY_COUNT, MAX_MEMORY_PAGES, MAX_PLAN_OUTPUT_BYTES, MAX_PLAN_RESOURCES,
    MAX_TABLE_COUNT, MAX_TABLE_ELEMENTS, MAX_WASM_STACK_BYTES, WASMTIME_VERSION, engine_features,
};

/// Separates this fingerprint from any other use of a SHA-256, so a digest
/// computed here can never be mistaken for one computed for another purpose.
const FINGERPRINT_DOMAIN: &[u8] = b"zup.plugin.engine-configuration.v1";

/// The identity of one engine configuration.
///
/// Hash rather than a description because the answer is only ever compared, and
/// a hash cannot be wrong in the way a list of settings can be: adding a limit
/// changes the value whether or not anybody remembered to update a comment.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct EngineFingerprint([u8; 32]);

impl EngineFingerprint {
    /// The raw digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The digest as hex, which is how an artifact records it.
    pub fn to_hex(&self) -> String {
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
        }
        output
    }
}

impl fmt::Debug for EngineFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("EngineFingerprint")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for EngineFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

/// What an engine configured for `target` is, as one comparable value.
pub fn engine_fingerprint(target: &str) -> EngineFingerprint {
    let mut hasher = Sha256::new();
    update_field(&mut hasher, FINGERPRINT_DOMAIN);
    update_field(&mut hasher, target.as_bytes());
    update_field(&mut hasher, WASMTIME_VERSION.as_bytes());
    update_field(&mut hasher, &wit_package_digest());
    update_number(&mut hasher, engine_features().bits());
    update_number(
        &mut hasher,
        u64::try_from(MAX_WASM_STACK_BYTES).expect("the Wasm stack limit fits in u64"),
    );
    update_number(&mut hasher, MAX_FUEL_PER_CALL);
    update_number(&mut hasher, EPOCH_DEADLINE_TICKS);
    update_number(&mut hasher, INVOCATION_DEADLINE_MILLIS);
    update_number(&mut hasher, MAX_HOST_CALLS);
    update_number(&mut hasher, 1);
    update_number(&mut hasher, MAX_MEMORY_PAGES);
    update_number(
        &mut hasher,
        u64::try_from(MAX_MEMORY_COUNT).expect("the memory count limit fits in u64"),
    );
    update_number(&mut hasher, MAX_TABLE_ELEMENTS);
    update_number(
        &mut hasher,
        u64::try_from(MAX_TABLE_COUNT).expect("the table count limit fits in u64"),
    );
    update_number(
        &mut hasher,
        u64::try_from(MAX_INSTANCES).expect("the instance limit fits in u64"),
    );
    update_number(
        &mut hasher,
        u64::try_from(MAX_PLAN_RESOURCES).expect("the resource output limit fits in u64"),
    );
    update_number(
        &mut hasher,
        u64::try_from(MAX_PLAN_OUTPUT_BYTES).expect("the byte output limit fits in u64"),
    );
    EngineFingerprint(hasher.finalize().into())
}

/// The digest of the canonical WIT contract.
///
/// Recorded in every artifact and checked when one is loaded, so a component
/// built against a different contract is refused rather than called with a
/// signature its author never wrote.
pub fn wit_package_digest() -> [u8; 32] {
    zup_plugin_abi::wit_package_digest()
}

fn update_field(hasher: &mut Sha256, value: &[u8]) {
    let length = u64::try_from(value.len()).expect("the fingerprint field length fits in u64");
    hasher.update(length.to_be_bytes());
    hasher.update(value);
}

fn update_number(hasher: &mut Sha256, value: u64) {
    update_field(hasher, &value.to_be_bytes());
}
