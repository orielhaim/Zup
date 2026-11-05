fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=TARGET");
    println!(
        "cargo::rustc-env=ZUP_BUILD_TARGET={}",
        std::env::var("TARGET").expect("Cargo did not set TARGET")
    );

    println!("cargo::rerun-if-env-changed=DEP_ZUP_PLUGIN_ABI_WIT_DIR");
    let directory = std::env::var("DEP_ZUP_PLUGIN_ABI_WIT_DIR").unwrap_or_else(|_| {
        panic!(
            "zup-plugin-abi did not publish its WIT directory; the host validates \
             against the canonical contract and cannot build without it"
        )
    });

    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(
        out.join("bindings.rs"),
        format!(
            "wasmtime::component::bindgen!({{\n    path: {directory:?},\n    \
             world: \"plugin\",\n}});\n"
        ),
    )
    .expect("write the generated bindings");
}
