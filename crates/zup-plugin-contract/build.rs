fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=TARGET");
    println!(
        "cargo::rustc-env=ZUP_BUILD_TARGET={}",
        std::env::var("TARGET").expect("Cargo did not set TARGET")
    );

    // Where the example plugin's component lives. `cargo xtask toolchain build`
    // writes it, and a test that reads it fails loudly rather than silently
    // against a component an earlier build left behind.
    //
    // Built by walking up rather than by writing `../../../`, because `..` on a
    // path that has already been joined does not survive the normalisation a
    // Windows path goes through, and the result is a path that reads as if it
    // exists and does not.
    let mut root =
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest"));
    root.pop();
    while root.file_name().is_some_and(|name| name != "crates") {
        if !root.pop() {
            break;
        }
    }
    root.pop();
    let component = root
        .join("examples")
        .join("plugins")
        .join("configure")
        .join("dist")
        .join("configure.wasm");
    println!("cargo::rustc-env=ZUP_TEST_PLUGIN={}", component.display());

    println!("cargo::rerun-if-env-changed=DEP_ZUP_PLUGIN_ABI_WIT_DIR");
    let directory = std::env::var("DEP_ZUP_PLUGIN_ABI_WIT_DIR").unwrap_or_else(|_| {
        panic!(
            "zup-plugin-abi did not publish its WIT directory; the host validates \
             against the canonical contract and cannot build without it"
        )
    });

    // The bindings macro takes a literal path and nothing else, so the invocation
    // is written here with the ABI crate's directory baked in. That is what keeps
    // one WIT file in the repository: the path is read from the ABI crate on
    // every build, so it cannot drift from it.
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
