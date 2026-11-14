use std::collections::BTreeMap;
use std::path::PathBuf;

use zup_core::{AppId, Frontend, ResourceKey, SelectedScope};
use zup_plan::{DesiredState, InstallationRecord, PlanRequest, PlannedInstallation};

use zup_plan::DeploymentEffects;
use zup_runtime::{BootstrapRequest, ContentAccess, OperationHost, attach_maintenance_copy};

use crate::bundle_packager::EmbeddedBundle;

pub enum WindowsContent<'a> {
    Embedded(&'a EmbeddedBundle),
    Acquired(
        &'a zup_artifact::VariantManifest,
        &'a zup_bundle::AcquiredPayloadSource,
    ),
}

pub struct WindowsOperationHost<'a> {
    pub app_id: AppId,
    pub scope: SelectedScope,
    pub frontend: Frontend,
    pub state_root: PathBuf,
    pub content: WindowsContent<'a>,
    pub acquire_prerequisites: bool,
}

impl OperationHost for WindowsOperationHost<'_> {
    fn collected_deployments(
        &self,
        input: &zup_runtime::OperationInput,
        record: &InstallationRecord,
    ) -> Result<Vec<zup_plan::RetiredDeploymentSelection>, String> {
        let remaining = self.cleanup_candidates(input, record, None)?;
        if self.scope == SelectedScope::Machine
            && !crate::transport::is_process_elevated().map_err(|error| error.to_string())?
        {
            return Ok(Vec::new());
        }
        Ok(zup_transaction::collected_deployments(record, &remaining))
    }

    fn cleanup_candidates(
        &self,
        input: &zup_runtime::OperationInput,
        record: &InstallationRecord,
        retiring: Option<&zup_plan::RetentionChange>,
    ) -> Result<Vec<zup_plan::CleanupOperation>, String> {
        let store = if self.scope == SelectedScope::Machine {
            if !crate::transport::is_process_elevated().map_err(|error| error.to_string())? {
                return Ok(Vec::new());
            }
            let machine = crate::machine_authority::MachineRoot::acquire(&self.app_id)
                .map_err(|error| error.to_string())?;
            if machine.path() != input.state_root {
                return Err("cleanup state root differs from machine authority".into());
            }
            machine.transaction_store()
        } else {
            zup_transaction::FilesystemTransactionStore::private_at(input.state_root)
        };
        let mut operations = zup_transaction::cleanup_candidates(&store, record, retiring)?;
        let state = crate::machine_state::StateAuthority {
            root: input.state_root.to_owned(),
            app: record.app_id.clone(),
            scope: record.scope,
        };
        operations.extend(
            zup_transaction::HistoryStorage(crate::filesystem::history::WindowsHistoryFilesystem(
                state,
            ))
            .candidates(&store, record, retiring)?
            .into_iter()
            .map(|history| zup_plan::CleanupOperation {
                candidate: zup_plan::CleanupCandidate::History(history),
                privilege: record.scope.authorization(),
            }),
        );
        Ok(operations)
    }

    fn data_bindings(
        &self,
        target: &zup_plan::BoundDeployment,
    ) -> Result<BTreeMap<zup_core::ResourceId, zup_plan::PersistentBinding>, String> {
        crate::resolve::resolve_persistent_bindings(
            target,
            &crate::host_dirs::WindowsInstallLocationResolver,
        )
    }

    fn persistent_claims(
        &self,
        input: &zup_runtime::OperationInput,
    ) -> Result<Option<zup_plan::PersistentClaimSelection>, zup_runtime::OperationError> {
        if self.scope == SelectedScope::Machine
            && !crate::transport::is_process_elevated()
                .map_err(|error| zup_runtime::OperationError::refused("authority", error))?
        {
            return Ok(None);
        }
        crate::ledger::InstallationRecordStore::new(input.state_root)
            .persistent_claims(
                &input.build.installer.app.id,
                self.scope,
                input.build.installer.target,
            )
            .map_err(|error| match error {
                crate::ledger::LedgerError::RecoveryRequired(transaction) => {
                    zup_runtime::OperationError::RecoveryRequired(transaction)
                }
                error => zup_runtime::OperationError::refused("persistent data", error),
            })
    }

    fn plan_build(
        &self,
        build: &zup_core::TargetBuildPlan,
        request: &PlanRequest,
        cancellation: &dyn zup_plan::CancellationQuery,
    ) -> Result<PlannedInstallation, zup_runtime::OperationError> {
        let planning = zup_core::BuildPlan {
            targets: vec![build.clone()],
        };
        let executor = match &self.content {
            WindowsContent::Embedded(bundle) => zup_plugin_runtime::WasmtimePluginExecutor::load(
                bundle.package().clone(),
                &build.installer.target,
            ),
            WindowsContent::Acquired(manifest, payload) => {
                zup_plugin_runtime::WasmtimePluginExecutor::load_acquired(
                    &manifest.plan,
                    payload,
                    &build.installer.target,
                )
            }
        }
        .map_err(|error| zup_runtime::OperationError::refused("plugin", error))?;
        let mut executor = zup_plugin_runtime::NativePluginExecutor {
            executor,
            locations: crate::WindowsInstallLocationResolver,
        };
        zup_plan::plan(&planning, request, Some(&mut executor), cancellation)
            .map_err(zup_runtime::OperationError::from)
    }

    fn resolve_target(&self, install: &DesiredState) -> Result<zup_plan::BoundDeployment, String> {
        crate::resolve::resolve_target(
            install,
            &crate::resolve::WindowsTargetContext::new(self.scope),
        )
        .map_err(|error| error.to_string())
    }

    fn observe(
        &self,
        target: Option<&zup_plan::BoundDeployment>,
        ledger: Option<&InstallationRecord>,
    ) -> Result<(Option<zup_plan::HostSnapshot>, BTreeMap<ResourceKey, bool>), String> {
        crate::planning::observe_target(target, ledger).map_err(|error| error.to_string())
    }

    fn compile(
        &self,
        execution: &zup_plan::ExecutionPlan,
        target: Option<&zup_plan::BoundDeployment>,
        ledger: Option<&InstallationRecord>,
    ) -> Result<DeploymentEffects, String> {
        crate::planning::transaction_input(
            execution,
            target,
            ledger,
            self.scope,
            &self.state_root,
            self.frontend,
            &self.app_id,
        )
        .map_err(|error| error.to_string())
    }

    fn attach_content(
        &self,
        target: &mut zup_plan::BoundDeployment,
        content: &ContentAccess,
        preset: Option<&zup_core::PresetRuntime>,
    ) -> Result<(), String> {
        let preparation = attach_maintenance_copy(target, content, "__zup_maintenance__.exe")
            .map_err(|error| error.to_string())?;
        if preparation == zup_plan::GenerationPreparation::ReuseVerified {
            if target.preset.as_ref().map(|installed| &installed.preset) != preset {
                return Err("repair requires the installed preset declaration".into());
            }
            return Ok(());
        }
        if let Some(preset) = preset {
            zup_runtime::attach_preset_content(target, preset, &|name| self.preset_bytes(name))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn prepare_bootstrap(
        &self,
        install: &DesiredState,
    ) -> Result<Option<BootstrapRequest>, String> {
        if !self.acquire_prerequisites {
            return Ok(None);
        }
        let bundle = match &self.content {
            WindowsContent::Embedded(bundle) => Some(*bundle),
            WindowsContent::Acquired(..) => None,
        };
        crate::bootstrap_plan::prepare(install, &self.state_root, self.scope, bundle)
            .map_err(|error| error.to_string())
    }
}

impl WindowsOperationHost<'_> {
    fn preset_bytes(&self, name: &str) -> Result<Vec<u8>, String> {
        match &self.content {
            WindowsContent::Embedded(bundle) => {
                if name == zup_bundle::PRESET_SOURCE {
                    return bundle
                        .preset()
                        .map(<[u8]>::to_vec)
                        .ok_or_else(|| "this installer was composed without a preset".to_owned());
                }
                let asset = ui_asset_name(name)
                    .ok_or_else(|| format!("`{name}` is not preset runtime content"))?;
                bundle
                    .ui_asset(asset)
                    .map(|(_, bytes)| bytes)
                    .map_err(|error| format!("the asset `{asset}`: {error}"))
            }
            WindowsContent::Acquired(manifest, payload) => {
                if name == zup_bundle::PRESET_SOURCE {
                    let digest = manifest
                        .preset
                        .as_ref()
                        .map(|preset| preset.digest)
                        .ok_or_else(|| {
                            "this release presents a window but carries no native image for it"
                                .to_owned()
                        })?;
                    return payload
                        .read_blob(&digest)
                        .map_err(|error| format!("the preset executable {digest}: {error}"));
                }
                let asset = ui_asset_name(name)
                    .ok_or_else(|| format!("`{name}` is not preset runtime content"))?;
                payload
                    .ui_asset(asset)
                    .map_err(|error| format!("the asset `{asset}`: {error}"))
            }
        }
    }
}

fn ui_asset_name(source: &str) -> Option<&str> {
    source
        .strip_prefix(zup_bundle::ASSET_SOURCE_PREFIX)
        .and_then(|rest| rest.strip_prefix('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::{
        App, AppId, Frontend, Install, InstallDirectory, InstallScope, Installer, NonEmptyString,
        PresetAsset, PresetRuntime, SelectedScope, Target, Template,
    };

    fn empty_target(scope: SelectedScope, id: &str) -> zup_plan::BoundDeployment {
        let target = Target::parse("windows-x64").expect("a target");
        DesiredState {
            generation_readers: Default::default(),
            data: Vec::new(),
            requirements: (),
            maintenance: None,
            installation: "00000000-0000-7000-8000-000000000001".parse().unwrap(),
            generation: None,
            app: App {
                id: AppId::new(id).expect("an app id"),
                name: NonEmptyString::new("Window").expect("a name"),
                version: semver::Version::parse("1.0.0").expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target,
            scope,
            install_directory: zup_core::TargetPath::new(target, r"C:\Windows\Temp\app")
                .expect("a target path"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),

            preset: None,
        }
        .bind_fixed()
        .expect("fixed deployment requirements")
    }

    fn preset() -> PresetRuntime {
        PresetRuntime {
            name: NonEmptyString::new("aurora").expect("a name"),
            version: semver::Version::parse("1.4.2").expect("a version"),
            protocol: zup_preset_protocol::PRESET_PROTOCOL_VERSION,
            required_capabilities: vec!["components".to_owned()],
            settings: serde_json::json!({ "hero": "Install Acme" }),
            assets: vec![PresetAsset {
                name: NonEmptyString::new("branding/logo.svg").expect("a name"),
                size: 6,
                sha256: zup_core::hash_bytes(b"<svg/>"),
            }],
        }
    }

    const LOGO: &[u8] = b"<svg/>";
    const PRESET_EXECUTABLE: &[u8] = b"the preset executable";

    fn prepared_target(root: &std::path::Path, scope: SelectedScope) -> zup_plan::BoundDeployment {
        let mut target = empty_target(scope, "com.example.acquired");
        let installer = root.join("runtime");
        std::fs::write(&installer, b"maintenance runtime").unwrap();
        attach_maintenance_copy(
            &mut target,
            &ContentAccess {
                installer: &installer,
                state_root: root,
                scope,
                action: zup_plan::LifecycleAction::Install,
                prior: None,
                maintenance: None,
            },
            "__zup_maintenance__.exe",
        )
        .unwrap();
        target
    }

    #[rstest::rstest]
    #[case(SelectedScope::User)]
    #[case(SelectedScope::Machine)]
    fn acquired_preset_joins_the_verified_maintenance_generation(#[case] scope: SelectedScope) {
        let root = tempfile::tempdir().unwrap();
        let (manifest, payload) = acquired_release(root.path(), zup_core::hash_bytes(LOGO));
        let host = WindowsOperationHost {
            app_id: AppId::new("com.example.acquired").unwrap(),
            scope,
            frontend: Frontend::Gui,
            state_root: root.path().join("state"),
            content: WindowsContent::Acquired(&manifest, &payload),
            acquire_prerequisites: false,
        };
        let mut target = prepared_target(root.path(), scope);
        zup_runtime::attach_preset_content(&mut target, &preset(), &|name| host.preset_bytes(name))
            .unwrap();
        target.validate_resolved().unwrap();
        assert!(target.files.is_empty());
        let generation = target.maintenance.unwrap();
        assert_eq!(generation.descriptor.files().len(), 3);
        for (artifact, bytes) in [
            (zup_core::MaintenanceArtifact::Preset, PRESET_EXECUTABLE),
            (
                zup_core::MaintenanceArtifact::Asset {
                    name: zup_core::RelativePath::new("branding/logo.svg").unwrap(),
                },
                LOGO,
            ),
        ] {
            let digest = zup_core::hash_bytes(bytes);
            let path = artifact.member_path(target.target, digest);
            assert!(
                generation
                    .descriptor
                    .files()
                    .iter()
                    .any(|file| file.path == path
                        && file.sha256 == digest
                        && file.size == bytes.len() as u64)
            );
            assert_eq!(
                generation.sources[&path].key,
                ResourceKey::File {
                    id: zup_core::FileResourceId::Maintenance { artifact }
                }
            );
        }
    }

    #[rstest::rstest]
    #[case::missing_executable(true)]
    #[case::missing_asset(false)]
    fn unavailable_authenticated_preset_content_is_refused(#[case] executable: bool) {
        let root = tempfile::tempdir().unwrap();
        let (mut manifest, payload) = acquired_release(
            root.path(),
            if executable {
                zup_core::hash_bytes(LOGO)
            } else {
                zup_core::hash_bytes(b"never acquired")
            },
        );
        if executable {
            manifest.preset = Some(zup_artifact::Descriptor::of(
                zup_artifact::MediaType::PRESET,
                b"missing executable",
            ));
        }
        let host = WindowsOperationHost {
            app_id: AppId::new("com.example.acquired").unwrap(),
            scope: SelectedScope::User,
            frontend: Frontend::Gui,
            state_root: root.path().join("state"),
            content: WindowsContent::Acquired(&manifest, &payload),
            acquire_prerequisites: false,
        };
        let mut target = prepared_target(root.path(), SelectedScope::User);
        let before = target.maintenance.clone();
        assert!(
            zup_runtime::attach_preset_content(&mut target, &preset(), &|name| host
                .preset_bytes(name))
            .is_err()
        );
        assert_eq!(target.maintenance, before);
        assert!(target.preset.is_none());
    }

    fn acquired_release(
        root: &std::path::Path,
        asset_digest: zup_core::Sha256Digest,
    ) -> (
        zup_artifact::VariantManifest,
        zup_bundle::AcquiredPayloadSource,
    ) {
        let preset = zup_core::PresetRuntime {
            assets: vec![zup_core::PresetAsset {
                name: NonEmptyString::new("branding/logo.svg").expect("a name"),
                size: LOGO.len() as u64,
                sha256: asset_digest,
            }],
            ..preset()
        };
        let plan = zup_core::PackagePlan {
            installer: Installer {
                app: App {
                    id: AppId::new("com.example.acquired").expect("a valid id"),
                    name: NonEmptyString::new("Acquired").expect("a name"),
                    version: "1.0.0".parse().expect("a version"),
                    publisher: None,
                    main: None,
                    description: None,
                },
                target: Target::parse("windows-x64").expect("a target"),
                frontend: Frontend::Gui,
                preset: Some(preset.clone()),
                updates: None,
                install: Install {
                    generation_readers: Default::default(),
                    data: Vec::new(),
                    scope: InstallScope::User,
                    directory: InstallDirectory {
                        user: Some(Template::parse("${install}").expect("a dir")),
                        machine: None,
                    },
                    allow_directory_override: false,
                },
                prerequisites: Vec::new(),
                components: Vec::new(),
                component_groups: Vec::new(),
                plugins: Vec::new(),
                files: Vec::new(),
                launchers: Vec::new(),
                path: Vec::new(),
                services: Vec::new(),
                protocols: Vec::new(),
                file_associations: Vec::new(),
            },
            entries: Vec::new(),
            prerequisite_artifacts: Vec::new(),
            ui_assets: vec![zup_core::PresetAsset {
                name: NonEmptyString::new("branding/logo.svg").expect("a name"),
                size: LOGO.len() as u64,
                sha256: asset_digest,
            }],
            plugins: Vec::new(),
            total_size: 0,
        };
        let package = zup_bundle::Package::parse(
            zup_bundle::BundleWriter::encode_plan_only_plan(&plan, &[]).expect("encodes"),
        )
        .expect("a plan-only package parses");

        let mut manifest = zup_artifact::VariantManifest {
            schema: zup_artifact::VARIANT_MANIFEST_SCHEMA,
            required_features: zup_artifact::FEATURE_VARIANT_MANIFESTS,
            target: Target::parse("windows-x64").expect("a target"),
            platform: zup_artifact::Platform::from_target(
                &Target::parse("windows-x64").expect("a target"),
            ),
            frontend: Frontend::Gui,
            runtime: Some(zup_artifact::Descriptor::of(
                zup_artifact::MediaType::RUNTIME,
                b"a runtime image",
            )),
            preset: Some(zup_artifact::Descriptor::of(
                zup_artifact::MediaType::PRESET,
                PRESET_EXECUTABLE,
            )),
            requirements: zup_artifact::VariantRequirements::default(),
            plan,
            logical_size: 0,
        };
        manifest.logical_size = manifest
            .runtime
            .iter()
            .chain(manifest.preset.as_ref())
            .map(|image| image.size)
            .sum();
        let manifest = zup_artifact::VariantManifest::parse(
            &serde_json::to_vec(&manifest).expect("serializes"),
        )
        .expect("a manifest this crate wrote is one it reads");

        let cache =
            zup_acquire::ContentCache::open(root.join("cache"), zup_acquire::CachePolicy::Auto)
                .expect("the cache opens");
        let mut entries = Vec::new();
        for content in [PRESET_EXECUTABLE, LOGO] {
            let descriptor = zup_acquire::ContentDescriptor::stored(
                zup_acquire::ContentKind::Payload,
                zup_core::hash_bytes(content),
                content.len() as u64,
            );
            let mut writer = cache.writer(&descriptor).expect("the writer opens");
            writer.write(content).expect("the bytes land");
            writer.commit().expect("and the blob proves itself");
            entries.push(zup_acquire::CatalogEntry::stored(
                zup_core::hash_bytes(content),
                content.len() as u64,
            ));
        }
        entries.sort_by_key(|entry| entry.digest);
        let catalog = zup_acquire::ContentCatalog::new(entries).expect("a well formed catalog");
        (
            manifest,
            zup_bundle::AcquiredPayloadSource::new(cache, catalog, &package),
        )
    }
}
