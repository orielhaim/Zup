use std::path::{Path, PathBuf};
use std::sync::Arc;

use zup_core::SelectedScope;
use zup_runtime::{PreparedOperation, RuntimeBackend};
use zup_session::{SessionDiscovery, SessionHost, SessionLaunch};

use crate::operation_host::LinuxOperationHost;

#[derive(Clone)]
pub struct LinuxSessionHost {
    installer: PathBuf,
}

impl LinuxSessionHost {
    pub fn open(installer: PathBuf) -> Self {
        Self { installer }
    }

    fn operation_host(&self, scope: SelectedScope) -> LinuxOperationHost {
        let machine = match scope {
            SelectedScope::Machine => Some(crate::operation_host::MachineHost {
                roots: crate::locations::MachineRoots::production(),
                systemd: crate::machine::SystemdRoots::production(),
            }),
            SelectedScope::User => None,
        };
        LinuxOperationHost { machine }
    }
}

impl SessionHost for LinuxSessionHost {
    type Bound<'a>
        = LinuxOperationHost
    where
        Self: 'a;

    fn bind<'a>(
        &'a self,
        context: &'a zup_session::OperationContext,
    ) -> Result<Self::Bound<'a>, String> {
        Ok(self.operation_host(context.scope))
    }

    fn discover(
        &mut self,
        executable: &Path,
        launch: &SessionLaunch,
    ) -> Result<SessionDiscovery, String> {
        self.installer = executable.to_path_buf();
        let carrier =
            crate::carrier::Carrier::open(executable).map_err(|error| error.to_string())?;
        let build = carrier
            .package()
            .build_plan()
            .map_err(|error| error.to_string())?
            .targets
            .into_iter()
            .next()
            .ok_or_else(|| "an installer package holds exactly one target".to_owned())?;
        let mut installations = Vec::new();
        for scope in [SelectedScope::User, SelectedScope::Machine] {
            let permitted = match scope {
                SelectedScope::User => build.installer.install.scope.allows_user(),
                SelectedScope::Machine => build.installer.install.scope.allows_machine(),
            };
            if !permitted {
                continue;
            }
            let state_root = match &launch.state_root {
                Some(root) => root.clone(),
                None => crate::paths::state_root(scope).map_err(|error| error.to_string())?,
            };
            let ledger = crate::ledger::LinuxLedgerStore::new(&state_root)
                .load(&build.installer.app.id, scope)
                .map_err(|error| error.to_string())?;
            installations.push(zup_session::ScopedInstallation {
                scope,
                state_root,
                ledger,
            });
        }
        Ok(SessionDiscovery {
            build,
            installations,
        })
    }

    fn backend(
        &self,
        context: &zup_session::OperationContext,
        prepared: &PreparedOperation,
    ) -> Result<Arc<dyn RuntimeBackend>, String> {
        let target = prepared
            .target_plan()
            .ok_or_else(|| "a Linux transaction resolves its target".to_owned())?;
        let generated = match prepared.install_plan() {
            Some(install) => {
                let generated = crate::integration::generated_map(install)
                    .map_err(|error| error.to_string())?;
                crate::integration::save_generated(
                    &context.state_root,
                    &install.app.id,
                    context.scope,
                    &generated,
                )
                .map_err(|error| error.to_string())?;
                generated
            }
            None => crate::integration::load_generated(
                &context.state_root,
                &target.app.id,
                context.scope,
            )
            .map_err(|error| error.to_string())?,
        };
        Ok(Arc::new(crate::backend::LinuxRuntimeBackend {
            installer: self.installer.clone(),
            operation: prepared.operation,
            selection: prepared.selection.clone(),
            install_dir_override: prepared.selection.install_directory.clone(),
            generated,
        }))
    }

    fn materialize_preset(
        &self,
        discovery: &SessionDiscovery,
        context: &zup_session::OperationContext,
    ) -> Result<zup_session::PresetLaunch, String> {
        let installer = discovery.installer();
        let installed = context.ledger.is_some();
        let capabilities = zup_preset_host::surface::capabilities(installer, installed);
        let runtime = match context.ledger.as_ref() {
            Some(ledger) => ledger
                .preset()
                .map(|recorded| recorded.preset.clone())
                .ok_or_else(|| {
                    format!(
                        "{} is installed with no recorded window, and its maintenance runtime \
                         cannot present one.\n\nReinstall it to restore the window it was \
                         installed with.",
                        installer.app.name
                    )
                })?,
            None => installer
                .preset
                .clone()
                .ok_or_else(|| "this installer was composed without a preset".to_owned())?,
        };
        zup_preset_host::process::check_presentable(&runtime, &capabilities)
            .map_err(|error| error.to_string())?;
        let (executable, assets, stage) = match context.ledger.as_ref() {
            Some(ledger) => {
                let recorded = ledger.preset().ok_or_else(|| {
                    format!(
                        "{} is installed with no recorded window.",
                        installer.app.name
                    )
                })?;
                let directory = zup_transaction::maintenance_directory(
                    &context.state_root,
                    &installer.app.id,
                    context.scope,
                    &ledger.version,
                );
                let resolved = zup_bundle::resolve_preset_content(
                    &directory,
                    recorded,
                    installer.target.executable_suffix(),
                    |path| path.to_string_lossy().into_owned(),
                )
                .map_err(|error| error.to_string())?;
                (resolved.executable, resolved.assets, None)
            }
            None => {
                let carrier = crate::carrier::Carrier::open(&self.installer)
                    .map_err(|error| error.to_string())?;
                let executable_name = format!("preset{}", installer.target.executable_suffix());
                let mut files = vec![(
                    executable_name.clone(),
                    carrier
                        .preset()
                        .ok_or_else(|| "this installer was composed without a preset".to_owned())?
                        .to_vec(),
                )];
                for asset in &runtime.assets {
                    let (_, bytes) = carrier
                        .package()
                        .ui_asset(asset.name.as_str())
                        .map_err(|error| format!("the asset `{}`: {error}", asset.name))?;
                    files.push((asset.name.to_string(), bytes));
                }
                let staged = stage_session(&files).map_err(|error| error.to_string())?;
                let executable = staged.executable.clone();
                let assets = staged.assets.clone();
                (
                    executable,
                    assets,
                    Some(zup_preset_host::PresetStage::acquire(staged.root)),
                )
            }
        };
        Ok(zup_session::PresetLaunch {
            executable,
            configuration: zup_preset_protocol::Configuration {
                settings: runtime.settings.clone(),
                assets,
            },
            capabilities,
            product: zup_preset_host::surface::product(installer),
            environment: zup_preset_protocol::environment::current(installer.target.as_str()),
            stage,
        })
    }

    fn copy_diagnostics(&self, text: &str) {
        for (program, args) in [
            ("wl-copy", vec![] as Vec<&str>),
            ("xclip", vec!["-selection", "clipboard"]),
            ("xsel", vec!["--clipboard", "--input"]),
        ] {
            if std::process::Command::new(program)
                .args(&args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write as _;
                    child
                        .stdin
                        .as_mut()
                        .ok_or_else(|| std::io::Error::other("no stdin"))?
                        .write_all(text.as_bytes())?;
                    child.wait()
                })
                .is_ok()
            {
                return;
            }
        }
    }
}

struct StagedSession {
    root: PathBuf,
    executable: PathBuf,
    assets: std::collections::BTreeMap<String, String>,
}

fn stage_session(files: &[(String, Vec<u8>)]) -> Result<StagedSession, crate::error::PathError> {
    let parent = std::env::temp_dir();
    let parent = crate::fs::OwnedDirectory::open(&parent)?;
    let name = format!(
        "zup-preset-{:x}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let directory = parent.create_exclusive_directory(&name, crate::fs::STATE_DIRECTORY_MODE)?;
    let root = directory.path().to_path_buf();
    let staged = (|| {
        let mut executable = None;
        let mut assets = std::collections::BTreeMap::new();
        for (name, bytes) in files {
            let path = write_tree(&directory, name, bytes)?;
            zup_preset_host::confirm_bytes(&path, bytes).map_err(|error| {
                crate::error::PathError::Refused {
                    path: path.display().to_string(),
                    reason: error.to_string(),
                }
            })?;
            if executable.is_none() {
                executable = Some(path);
            } else {
                assets.insert(name.clone(), path.to_string_lossy().into_owned());
            }
        }
        Ok(StagedSession {
            root: root.clone(),
            executable: executable.ok_or_else(|| crate::error::PathError::Refused {
                path: directory.path().display().to_string(),
                reason: "the staged preset has no executable".into(),
            })?,
            assets,
        })
    })();
    if staged.is_err() {
        drop(directory);
        let _ = std::fs::remove_dir_all(root);
    }
    staged
}

fn write_tree(
    directory: &crate::fs::OwnedDirectory,
    relative: &str,
    bytes: &[u8],
) -> Result<PathBuf, crate::error::PathError> {
    use rustix::fs::Mode;

    let names: Vec<&str> = relative
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    if names.is_empty() || names.iter().any(|part| *part == "." || *part == "..") {
        return Err(crate::error::PathError::Refused {
            path: relative.into(),
            reason: "a staged preset path stays inside its directory".into(),
        });
    }
    let file_name = names[names.len() - 1];
    let mut current = directory.duplicate()?;
    for name in &names[..names.len() - 1] {
        current = match current.kind_or_absent(name)? {
            None => current.create_exclusive_directory(name, crate::fs::STATE_DIRECTORY_MODE)?,
            Some(crate::fs::EntryKind::Directory) => {
                let (child, _, _) = current.open_child_verified(name)?;
                child
            }
            Some(_) => {
                return Err(crate::error::PathError::UnexpectedKind {
                    path: current.path().join(name).display().to_string(),
                    expected: "a directory",
                });
            }
        };
    }
    let mode = Mode::from_bits_truncate(0o700);
    current.write_durable(file_name, bytes, mode)?;
    let streamed = current.stream_regular(file_name)?;
    if streamed.sha256 != zup_core::hash_bytes(bytes) {
        return Err(crate::error::PathError::Refused {
            path: current.path().join(file_name).display().to_string(),
            reason: "staged content does not match the verified preset".into(),
        });
    }
    Ok(current.path().join(file_name))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::write_tree;

    #[test]
    fn a_symlink_is_replaced_instead_of_followed() {
        let root = std::env::temp_dir().join(format!("zup-stage-{}-symlink", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let directory =
            crate::fs::OwnedDirectory::create(&root, crate::fs::STATE_DIRECTORY_MODE).unwrap();
        let sentinel = root.join("sentinel");
        std::fs::write(&sentinel, b"keep").unwrap();
        symlink(&sentinel, root.join("preset")).unwrap();
        let written = write_tree(&directory, "preset", b"real").unwrap();
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
        assert_eq!(std::fs::read(written).unwrap(), b"real");
        drop(directory);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_sessions_do_not_share_a_staging_directory() {
        let first = super::stage_session(&[("preset".into(), b"one".to_vec())]).unwrap();
        let second = super::stage_session(&[("preset".into(), b"two".to_vec())]).unwrap();
        assert_ne!(first.root, second.root);
        assert_eq!(std::fs::read(&first.executable).unwrap(), b"one");
        std::fs::write(&first.executable, b"tampered").unwrap();
        assert!(zup_preset_host::confirm_bytes(&first.executable, b"one").is_err());
        let root = first.root.clone();
        drop(zup_preset_host::PresetStage::acquire(first.root));
        assert!(!root.exists());
        assert!(second.root.exists());
        drop(zup_preset_host::PresetStage::acquire(second.root));
    }
}
