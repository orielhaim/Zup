use std::collections::BTreeMap;
use std::path::PathBuf;

use zup_core::{AppId, Frontend, ResourceKey, SelectedScope};
use zup_exec::InstallLedger;
use zup_plan::{InstallPlan, PlanRequest, PlannedInstallation};
use zup_platform::TargetPlan;
use zup_runtime::{BootstrapRequest, ContentAccess, OperationHost, attach_maintenance_copy};
use zup_transaction::TransactionInput;

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
    fn plan_build(
        &self,
        build: &zup_core::TargetBuildPlan,
        request: &PlanRequest,
        cancellation: &dyn zup_plan::CancellationQuery,
    ) -> Result<PlannedInstallation, String> {
        let planning = zup_core::BuildPlan {
            targets: vec![build.clone()],
        };
        match &self.content {
            WindowsContent::Embedded(bundle) => {
                let mut executor = zup_plugin_runtime::WasmtimePluginExecutor::load(
                    bundle.package().clone(),
                    &build.installer.target,
                )
                .map_err(|error| error.to_string())?;
                zup_plan::plan_with_plugins(&planning, request, &mut executor, cancellation)
                    .map_err(|error| error.to_string())
            }
            WindowsContent::Acquired(..) => zup_plan::plan(&planning, request)
                .map(|plan| PlannedInstallation {
                    plan,
                    generated_files: Vec::new(),
                })
                .map_err(|error| error.to_string()),
        }
    }

    fn resolve_target(&self, install: &InstallPlan) -> Result<TargetPlan, String> {
        crate::resolve::resolve_target(
            install,
            &crate::resolve::WindowsTargetContext::new(self.scope),
        )
        .map_err(|error| error.to_string())
    }

    fn observe(
        &self,
        target: Option<&TargetPlan>,
        ledger: Option<&InstallLedger>,
    ) -> Result<(Option<zup_exec::HostSnapshot>, BTreeMap<ResourceKey, bool>), String> {
        crate::planning::observe_target(target, ledger).map_err(|error| error.to_string())
    }

    fn compile(
        &self,
        _action: zup_exec::LifecycleAction,
        execution: &zup_exec::ExecutionPlan,
        target: Option<&TargetPlan>,
        ledger: Option<&InstallLedger>,
    ) -> Result<TransactionInput, String> {
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
        target: &mut TargetPlan,
        content: &ContentAccess,
        preset: Option<&zup_core::PresetRuntime>,
    ) -> Result<(), String> {
        let state_root = PathBuf::from(crate::plain_path_text(content.state_root));
        let content = ContentAccess {
            installer: content.installer,
            state_root: &state_root,
            scope: content.scope,
        };
        attach_maintenance_copy(
            target,
            content.installer,
            content.state_root,
            content.scope,
            "__zup_maintenance__.exe",
        )
        .map_err(|error| error.to_string())?;
        if let Some(preset) = preset {
            let runtime_directory = zup_transaction::maintenance_directory(
                content.state_root,
                &self.app_id,
                content.scope,
                &target.app.version,
            );
            attach_ui_runtime(target, &runtime_directory, content.scope, preset, &|name| {
                self.preset_bytes(name)
            })?;
        }
        Ok(())
    }

    fn prepare_bootstrap(&self, install: &InstallPlan) -> Result<Option<BootstrapRequest>, String> {
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

fn attach_ui_runtime(
    target: &mut TargetPlan,
    runtime_directory: &std::path::Path,
    scope: SelectedScope,
    preset: &zup_core::PresetRuntime,
    bytes: &impl Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    let executable = bytes(zup_bundle::PRESET_SOURCE).map_err(|error| {
        format!(
            "this application presents the preset `{}`, and its executable could not be read: {error}",
            preset.name
        )
    })?;
    let executable_digest = zup_core::hash_bytes(&executable);
    let mut files = vec![(
        zup_bundle::preset_path(
            runtime_directory,
            &executable_digest,
            target.target.executable_suffix(),
        ),
        executable_digest,
        executable.len() as u64,
        zup_core::RelativePath::new(zup_bundle::PRESET_SOURCE)
            .expect("a reserved source name is always relative"),
        true,
    )];
    for asset in &preset.assets {
        let content = bytes(zup_bundle::asset_source_name(asset.name.as_str()).as_str())
            .map_err(|error| format!("the asset `{}` could not be read: {error}", asset.name))?;
        if content.len() as u64 != asset.size || zup_core::hash_bytes(&content) != asset.sha256 {
            return Err(format!(
                "the asset `{}` is not the content this application configured",
                asset.name
            ));
        }
        files.push((
            zup_bundle::asset_path(runtime_directory, asset.name.as_str(), &asset.sha256),
            asset.sha256,
            asset.size,
            zup_bundle::asset_source_name(asset.name.as_str()),
            false,
        ));
    }

    for (path, digest, size, source, executable) in files {
        let destination =
            zup_platform::TargetPath::new(target.target, crate::plain_path_text(&path))
                .map_err(|error| format!("preset runtime destination: {error}"))?;
        let text = destination.to_string();
        target.files.push(zup_platform::TargetFile {
            key: zup_core::ResourceKey::File { destination: text },
            source_relative: source,
            destination,
            size,
            sha256: digest,
            privilege: scope.authorization(),
            executable,
        });
        target.summary.file_count += 1;
        target.summary.resource_count += 1;
        target.summary.install_bytes = target.summary.install_bytes.saturating_add(size);
    }
    target.preset = Some(zup_core::InstalledPreset {
        preset: preset.clone(),
        executable: executable_digest,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::{
        App, AppId, Frontend, Install, InstallDirectory, InstallScope, Installer, NonEmptyString,
        PresetAsset, PresetRuntime, SelectedScope, Target, Template,
    };

    fn empty_target(scope: SelectedScope, id: &str) -> TargetPlan {
        let target = Target::parse("windows-x64").expect("a target");
        TargetPlan {
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
            install_directory: zup_platform::TargetPath::new(target, r"C:\Windows\Temp\app")
                .expect("a target path"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
            summary: zup_platform::TargetPlanSummary {
                file_count: 0,
                install_bytes: 0,
                resource_count: 0,
                requires_authorization: false,
                selected_component_count: 0,
                prerequisite_count: 0,
                download_bytes: 0,
            },
            preset: None,
        }
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

    #[test]
    fn a_window_is_attached_as_scope_aware_installed_content() {
        let root = tempfile::tempdir().expect("a scratch directory");
        let app = AppId::new("com.example.window").expect("an app id");
        let version = semver::Version::parse("1.0.0").expect("a version");
        let source = |name: &str| -> Result<Vec<u8>, String> {
            Ok(if name == zup_bundle::PRESET_SOURCE {
                PRESET_EXECUTABLE.to_vec()
            } else {
                LOGO.to_vec()
            })
        };

        let mut seen = Vec::new();
        for scope in [SelectedScope::User, SelectedScope::Machine] {
            let state_root = root.path().join(match scope {
                SelectedScope::User => "user",
                SelectedScope::Machine => "machine",
            });
            let directory =
                zup_transaction::maintenance_directory(&state_root, &app, scope, &version);
            let mut target = empty_target(scope, "com.example.window");
            attach_ui_runtime(&mut target, &directory, scope, &preset(), &source)
                .expect("the window is attached");

            let runtime = target.preset.as_ref().expect("the plan records the window");
            assert_eq!(
                runtime.preset,
                preset(),
                "the settings and the asset are the ones the application configured, verbatim"
            );
            assert_eq!(
                runtime.executable,
                zup_core::hash_bytes(PRESET_EXECUTABLE),
                "and the executable is named by content"
            );

            let executable = zup_bundle::preset_path(
                &directory,
                &runtime.executable,
                Target::parse("windows-x64")
                    .expect("a target")
                    .executable_suffix(),
            );
            let asset = zup_bundle::asset_path(
                &directory,
                "branding/logo.svg",
                &zup_core::hash_bytes(LOGO),
            );
            assert_eq!(
                target.files.len(),
                2,
                "the executable and the asset, and nothing else"
            );
            let destinations: Vec<String> = target
                .files
                .iter()
                .map(|file| file.destination.as_str().to_owned())
                .collect();
            assert!(
                destinations.contains(&crate::plain_path_text(&executable)),
                "the preset executable is at {}",
                executable.display()
            );
            assert!(
                destinations.contains(&crate::plain_path_text(&asset)),
                "the asset is at {}",
                asset.display()
            );
            for file in &target.files {
                assert_eq!(
                    file.privilege,
                    scope.authorization(),
                    "a {scope} window's content is installed content of that scope"
                );
            }
            assert_eq!(
                target.files[0].source_relative.as_str(),
                zup_bundle::PRESET_SOURCE,
                "and the plan names where the bytes come from rather than a file to copy"
            );
            assert_eq!(
                target.summary.file_count, 2,
                "the content counts as content"
            );
            seen.push(executable);
        }
        assert_ne!(seen[0], seen[1], "two scopes are two storages");
    }

    #[test]
    fn a_window_whose_executable_cannot_be_read_is_refused() {
        let root = tempfile::tempdir().expect("a scratch directory");
        let mut target = empty_target(SelectedScope::User, "com.example.unreadable");
        let unreadable = |_: &str| -> Result<Vec<u8>, String> {
            Err("this payload carries no preset".to_owned())
        };
        let error = attach_ui_runtime(
            &mut target,
            &root.path().join("maintenance"),
            SelectedScope::User,
            &preset(),
            &unreadable,
        )
        .expect_err("a window with no executable cannot be installed");
        assert!(error.contains("could not be read"), "{error}");
        assert!(
            target.preset.is_none(),
            "and the plan records no window rather than one it cannot honour"
        );
        assert!(target.files.is_empty(), "and no half-installed content");
    }

    #[test]
    fn an_asset_that_is_not_the_configured_content_is_refused() {
        let root = tempfile::tempdir().expect("a scratch directory");
        let mut target = empty_target(SelectedScope::User, "com.example.mismatch");
        let source = |name: &str| -> Result<Vec<u8>, String> {
            Ok(if name == zup_bundle::PRESET_SOURCE {
                PRESET_EXECUTABLE.to_vec()
            } else {
                b"something else".to_vec()
            })
        };
        let error = attach_ui_runtime(
            &mut target,
            &root.path().join("maintenance"),
            SelectedScope::User,
            &preset(),
            &source,
        )
        .expect_err("an asset that is not what the application configured");
        assert!(
            error.contains("branding/logo.svg"),
            "and it names the asset: {error}"
        );
        assert!(target.preset.is_none(), "and records no window");
    }

    #[test]
    fn a_windows_bytes_come_from_verified_acquired_content() {
        let root = tempfile::tempdir().expect("a scratch directory");
        let (manifest, payload) = acquired_release(root.path(), zup_core::hash_bytes(LOGO));
        let mut target = empty_target(SelectedScope::User, "com.example.acquired");
        let directory = zup_transaction::maintenance_directory(
            &root.path().join("state"),
            &AppId::new("com.example.acquired").expect("a valid id"),
            SelectedScope::User,
            &target.app.version,
        );
        let host = WindowsOperationHost {
            app_id: AppId::new("com.example.acquired").expect("a valid id"),
            scope: SelectedScope::User,
            frontend: Frontend::Gui,
            state_root: root.path().join("state"),
            content: WindowsContent::Acquired(&manifest, &payload),
            acquire_prerequisites: false,
        };

        attach_ui_runtime(
            &mut target,
            &directory,
            SelectedScope::User,
            &preset(),
            &|name| host.preset_bytes(name),
        )
        .expect("a release that carries the window can install it");

        let recorded = target.preset.as_ref().expect("the plan records the window");
        assert_eq!(
            recorded.executable,
            zup_core::hash_bytes(PRESET_EXECUTABLE),
            "named by the digest the release authenticated, not discovered by hashing"
        );
        assert_eq!(
            recorded.preset,
            preset(),
            "and the same window the build configured"
        );

        let mut destinations: Vec<String> = target
            .files
            .iter()
            .map(|file| file.destination.as_str().to_owned())
            .collect();
        destinations.sort();
        let mut expected = vec![
            crate::plain_path_text(&zup_bundle::asset_path(
                &directory,
                "branding/logo.svg",
                &recorded.preset.assets[0].sha256,
            )),
            crate::plain_path_text(&zup_bundle::preset_path(
                &directory,
                &recorded.executable,
                Target::parse("windows-x64")
                    .expect("a target")
                    .executable_suffix(),
            )),
        ];
        expected.sort();
        assert_eq!(
            destinations, expected,
            "installed as content-addressed files beside the maintenance runtime, the same \
             layout a composed install produces"
        );
        let mut sources: Vec<&str> = target
            .files
            .iter()
            .map(|file| file.source_relative.as_str())
            .collect();
        sources.sort();
        assert_eq!(
            sources,
            [
                zup_bundle::PRESET_SOURCE,
                "__zup_preset_asset__/branding/logo.svg",
            ],
            "and the plan names where the bytes come from, rather than a build-machine path"
        );
    }

    #[rstest::rstest]
    #[case::an_executable_the_cache_does_not_hold(true)]
    #[case::an_asset_the_cache_does_not_hold(false)]
    fn a_window_the_release_does_not_carry_is_refused(#[case] absent_executable: bool) {
        let root = tempfile::tempdir().expect("a scratch directory");
        let named = if absent_executable {
            zup_core::hash_bytes(LOGO)
        } else {
            zup_core::hash_bytes(b"never acquired")
        };
        let (mut manifest, payload) = acquired_release(root.path(), named);
        if absent_executable {
            let mut wrong = PRESET_EXECUTABLE.to_vec();
            wrong.push(b'!');
            manifest.preset = Some(zup_artifact::Descriptor::of(
                zup_artifact::MediaType::PRESET,
                &wrong,
            ));
        }
        let mut target = empty_target(SelectedScope::User, "com.example.absent");
        let directory = root.path().join("maintenance");
        let host = WindowsOperationHost {
            app_id: AppId::new("com.example.absent").expect("a valid id"),
            scope: SelectedScope::User,
            frontend: Frontend::Gui,
            state_root: root.path().join("state"),
            content: WindowsContent::Acquired(&manifest, &payload),
            acquire_prerequisites: false,
        };

        let error = attach_ui_runtime(
            &mut target,
            &directory,
            SelectedScope::User,
            &preset(),
            &|name| host.preset_bytes(name),
        )
        .expect_err("a release that does not carry the window cannot install one");
        assert!(
            !error.is_empty(),
            "and the refusal says what is wrong: {error}"
        );
        assert!(target.preset.is_none(), "and the plan records no window");
        assert!(target.files.is_empty(), "and no half-installed content");
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
        let plan = zup_bundle::PortableBuildPlan {
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
