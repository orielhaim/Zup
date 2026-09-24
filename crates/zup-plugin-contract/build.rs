fn main() {
    println!("cargo::rerun-if-env-changed=TARGET");
    println!(
        "cargo::rustc-env=ZUP_BUILD_TARGET={}",
        std::env::var("TARGET").expect("Cargo did not set TARGET")
    );
}
