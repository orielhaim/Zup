#![cfg(feature = "plugin")]

use zup_sdk::plugin::prelude::*;

#[test]
fn a_plugin_plan_is_built_from_declarations() {
    let plan = Plan::new().generated_file(GeneratedFile::text(
        "${install}/installed-by.txt",
        "installed by test",
    ));
    let plan = plan.launcher(Launcher::menu("Test Settings", "${launcher}"));
    let _ = plan;
}

#[test]
fn a_plugin_error_names_itself() {
    let error = Error::unsupported("not here");
    assert_eq!(error.code(), "unsupported");
    assert!(error.message().contains("not here"));
}
