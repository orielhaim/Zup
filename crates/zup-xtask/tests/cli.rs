use std::process::{Command, Output};

use rstest::rstest;

fn xtask(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .output()
        .expect("xtask runs")
}

#[rstest]
#[case(&["verify-everything"], "verify-everything")]
#[case(&["emit-portable-matrix", "--matrix", "nope"], "nope")]
#[case(&["emit-host-packages"], "--host")]
#[case(&["toolchain", "package"], "--cli")]
fn rejected_arguments_are_usage_errors(#[case] args: &[&str], #[case] named: &str) {
    let output = xtask(args);
    assert_eq!(output.status.code(), Some(2), "{args:?}");
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains(named), "{args:?}");
}
