use std::path::{Path, PathBuf};

use zup_core::AppId;
use zup_plan::TargetBuildPlan;

pub fn current_executable() -> miette::Result<PathBuf> {
    zup_windows::current_exe().map_err(|error| miette::miette!("executable: {error}"))
}

pub fn open_bundle(executable: &Path) -> miette::Result<zup_windows::EmbeddedBundle> {
    zup_windows::EmbeddedBundle::open(executable)
        .map_err(|error| miette::miette!("installer package {}: {error}", executable.display()))
}

pub fn open_bundle_if_present(
    executable: &Path,
) -> miette::Result<Option<zup_windows::EmbeddedBundle>> {
    match zup_windows::EmbeddedBundle::open(executable) {
        Ok(bundle) => Ok(Some(bundle)),
        Err(error) if error.is_missing_resource() => Ok(None),
        Err(error) => Err(miette::miette!(
            "installer package {}: {error}",
            executable.display()
        )),
    }
}

pub fn target_plan(bundle: &zup_windows::EmbeddedBundle) -> miette::Result<TargetBuildPlan> {
    let build = bundle
        .build_plan()
        .map_err(|error| miette::miette!("installer package: {error}"))?;
    let mut targets = build.targets;
    if targets.len() != 1 {
        return Err(miette::miette!(
            "an installer package must contain exactly one target"
        ));
    }
    Ok(targets.remove(0))
}

pub fn maintenance_destination(
    state_root: &Path,
    app_id: &AppId,
    scope: zup_core::SelectedScope,
    version: &semver::Version,
    target: &zup_core::TargetTriple,
) -> miette::Result<zup_platform::TargetPath> {
    let path = zup_transaction::maintenance_runtime_path(
        state_root,
        app_id,
        scope,
        version,
        target.executable_suffix(),
    );
    zup_platform::TargetPath::new(target.clone(), zup_windows::plain_path_text(&path))
        .map_err(|error| miette::miette!("maintenance destination: {error}"))
}
