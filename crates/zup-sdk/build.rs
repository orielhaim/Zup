fn main() {
    println!("cargo::rerun-if-env-changed=DEP_ZUP_PLUGIN_ABI_WIT_DIR");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_PLUGIN");
    if std::env::var_os("CARGO_FEATURE_PLUGIN").is_none() {
        return;
    }
    let directory = std::env::var("DEP_ZUP_PLUGIN_ABI_WIT_DIR").unwrap_or_else(|_| {
        panic!(
            "zup-plugin-abi did not publish its WIT directory; the plugin role \
             generates its bindings from the canonical contract and cannot \
             build without it"
        )
    });
    println!("cargo::rustc-env=ZUP_PLUGIN_SDK_WIT_DIR={directory}");
}
