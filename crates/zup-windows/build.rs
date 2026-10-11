fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("win32.rs");

    windows_bindgen::bindgen([
        "--out",
        out.to_str().expect("utf-8 out path"),
        "--sys",
        "--filter",
        "SHGetKnownFolderPath",
        "--filter",
        "CoTaskMemFree",
    ]);
}
