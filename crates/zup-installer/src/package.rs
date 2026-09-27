//! The package this process carries.
//!
//! An installer image is a bare runtime template with one package compiled into
//! it: the plan, the content or the reference to content the release carries, and
//! whatever else the build embedded. Everything the runtime does starts by
//! reading that package, so the ways of reading it and the ways of failing to are
//! stated once here.

use std::path::{Path, PathBuf};

use zup_core::AppId;
use zup_plan::TargetBuildPlan;

/// The executable this process is running from.
///
/// `current_exe` rather than `argv[0]`: a runtime is relaunched by a dispatcher
/// and by an uninstall runner, and a relative or resolved-through-`PATH` argument
/// is not a trustworthy description of the file being replaced.
pub fn current_executable() -> miette::Result<PathBuf> {
    zup_windows::current_exe().map_err(|error| miette::miette!("executable: {error}"))
}

/// The package this image carries.
pub fn open_bundle(executable: &Path) -> miette::Result<zup_windows::EmbeddedBundle> {
    zup_windows::EmbeddedBundle::open(executable)
        .map_err(|error| miette::miette!("installer package: {error}"))
}

/// The package this image carries, or `None` when the image carries none.
///
/// A missing package is not a corrupt package. A bare runtime template, staged
/// beside its content package by a dispatcher, is a normal state; an unreadable
/// one is an error and is reported as one.
pub fn open_bundle_if_present(
    executable: &Path,
) -> miette::Result<Option<zup_windows::EmbeddedBundle>> {
    match zup_windows::EmbeddedBundle::open(executable) {
        Ok(bundle) => Ok(Some(bundle)),
        Err(error) if error.is_missing_resource() => Ok(None),
        Err(error) => Err(miette::miette!("installer package: {error}")),
    }
}

/// The one target this image installs.
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

/// Where an installation persists the maintenance runtime for one scope.
///
/// The triple is the target's own, because the destination is a `TargetPath`: a
/// maintenance copy is an owned resource of the same target as everything else it
/// sits beside, and the executor lowers it exactly as it lowers them.
pub fn maintenance_destination(
    state_root: &Path,
    app_id: &AppId,
    scope: zup_core::SelectedScope,
    version: &str,
    target: &zup_core::TargetTriple,
) -> miette::Result<zup_platform::TargetPath> {
    let path = zup_windows::maintenance_destination(state_root, app_id, scope, version);
    zup_platform::TargetPath::new(target.clone(), zup_windows::plain_path_text(&path))
        .map_err(|error| miette::miette!("maintenance destination: {error}"))
}
