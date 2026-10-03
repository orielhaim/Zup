//! Locate the canonical WIT this crate generates its bindings from.
//!
//! The plugin ABI crate owns the WIT file and publishes its directory as build
//! metadata. Reading the path from there rather than from a local `wit/`
//! directory is what lets a plugin author depend on the SDK and nothing else:
//! there is no copy of the contract in their project that could drift from the
//! one the host validates against.

fn main() {
    println!("cargo::rerun-if-env-changed=DEP_ZUP_PLUGIN_ABI_WIT_DIR");
    let directory = std::env::var("DEP_ZUP_PLUGIN_ABI_WIT_DIR").unwrap_or_else(|_| {
        panic!(
            "zup-plugin-abi did not publish its WIT directory; the plugin SDK \
             generates its bindings from the canonical contract and cannot \
             build without it"
        )
    });
    println!("cargo::rustc-env=ZUP_PLUGIN_SDK_WIT_DIR={directory}");
}
