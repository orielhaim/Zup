fn main() {
    println!("cargo::rerun-if-changed=wit/zup-plugin.wit");
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("wit");
    println!("cargo::metadata=wit_dir={}", directory.display());
}
