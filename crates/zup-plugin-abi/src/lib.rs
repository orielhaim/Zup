#![forbid(unsafe_code)]

mod limits;

pub use limits::{
    AOT_FORMAT_VERSION, EPOCH_DEADLINE_TICKS, INVOCATION_DEADLINE_MILLIS, MAX_AOT_BYTES,
    MAX_FUEL_PER_CALL, MAX_HOST_CALLS, MAX_INSTANCES, MAX_MEMORY_BYTES, MAX_MEMORY_COUNT,
    MAX_MEMORY_PAGES, MAX_PLAN_OUTPUT_BYTES, MAX_PLAN_RESOURCES, MAX_TABLE_COUNT,
    MAX_TABLE_ELEMENTS, MAX_WASM_STACK_BYTES, PLUGIN_API_VERSION, WASM_PAGE_BYTES,
    WASMTIME_VERSION, WIT_PACKAGE_VERSION,
};

use sha2::{Digest, Sha256};

pub const WIT_PACKAGE: &str = include_str!("../wit/zup-plugin.wit");

pub const WORLD: &str = "plugin";

pub const PLANNER_EXPORT_NAME: &str = "zup:plugin/planner@1.0.0";

pub const PLAN_FUNCTION_NAME: &str = "plan";

pub fn wit_package_digest() -> [u8; 32] {
    digest_canonical(WIT_PACKAGE)
}

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

    #[test]
    fn the_digest_is_the_one_artifacts_record() {
        assert_eq!(
            wit_package_digest().to_vec(),
            digest_canonical(WIT_PACKAGE),
            "the published digest and the published contract agree"
        );
    }
}
