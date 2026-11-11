pub use zup_update::content::*;

use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub state_root: Option<PathBuf>,
    pub action: Option<zup_plan::LifecycleAction>,
    pub enable: Vec<String>,
    pub disable: Vec<String>,
    pub install_directory: Option<PathBuf>,
    pub scope: Option<zup_core::SelectedScope>,
}

impl Request {
    pub fn new(action: zup_plan::LifecycleAction) -> Self {
        Self {
            action: Some(action),
            ..Self::default()
        }
    }

    pub fn components(mut self, enable: &[String], disable: &[String]) -> Self {
        self.enable = enable.to_vec();
        self.disable = disable.to_vec();
        self
    }
}

fn pinned_source(
    record: Option<&zup_plan::InstallationRecord>,
    operation: zup_runtime::Operation,
) -> Result<Option<&zup_core::ReleaseIdentity>, String> {
    match operation {
        zup_runtime::Operation::Repair { .. } | zup_runtime::Operation::Modify => record
            .and_then(zup_plan::InstallationRecord::release_identity)
            .map(Some)
            .ok_or_else(|| "the installed release has no authenticated source identity".into()),
        zup_runtime::Operation::Rollback { revision } => record
            .and_then(|record| {
                record
                    .retained
                    .iter()
                    .find(|snapshot| snapshot.revision == revision)
            })
            .and_then(|snapshot| snapshot.release.as_ref())
            .map(Some)
            .ok_or_else(|| {
                "the selected retained release has no authenticated source identity".into()
            }),
        _ => Ok(None),
    }
}

pub fn source_context(
    installer: &zup_core::Installer,
    record: Option<&zup_plan::InstallationRecord>,
    operation: zup_runtime::Operation,
    state_root: PathBuf,
) -> Result<zup_update::TrustContext, String> {
    let config = installer
        .updates
        .as_ref()
        .ok_or("this source has no authenticated update repository")?;
    let context = if let Some(identity) = pinned_source(record, operation)? {
        if identity.app_id != installer.app.id {
            return Err("the recorded source belongs to another application".into());
        }
        zup_update::TrustContext::from_release(
            config,
            identity,
            installer.install.scope,
            state_root,
        )
        .map_err(|error| error.to_string())?
    } else {
        zup_update::TrustContext::from_update_config(
            config,
            installer.app.id.clone(),
            installer.install.scope,
            state_root,
        )
    };
    Ok(context)
}

pub fn validate_selected_source(
    acquired: &Acquired,
    carrier: &zup_core::PackagePlan,
    record: Option<&zup_plan::InstallationRecord>,
    operation: zup_runtime::Operation,
) -> Result<(), String> {
    if let Some(identity) = pinned_source(record, operation)? {
        acquired
            .identity()
            .verify_source(identity)
            .map_err(|error| error.to_string())?;
    } else if &acquired.manifest.plan != carrier {
        return Err("the authenticated source differs from this installer's application contract; use its matching published installer or run an explicit update".into());
    }
    Ok(())
}
