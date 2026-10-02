//! The embedded-package lifecycle, tested against a real plan.
//!
//! The plan is written out literally rather than compiled from a manifest,
//! because the runtime never sees a manifest: it is handed a plan, and a fixture
//! that reached for the compiler to produce one would put a build-plane
//! dev-dependency into the package whose whole purpose is to have none.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::TempDir;
use zup_core::{
    App, AppId, FileMapping, Install, InstallDirectory, InstallScope, Installer, NonEmptyString,
    PluginBinding, PluginId, RelativePath, ResolvedFile, ResolvedPlugin, SelectedScope,
    TargetTriple, Template, UiAsset, UiPreset, hash_reader,
};
use zup_exec::LifecycleAction;
use zup_plan::{
    CancellationQuery, PluginExecutor, PluginFailure, PluginPlanningContext,
    PluginResourceProposal, TargetBuildPlan,
};

use super::{
    EmbeddedPreparation, Request, RuntimeCancellationQuery, acquired_ui_source, attach_ui_runtime,
    prepare_embedded_request,
};

/// The plugin the fixture declares, which no test here ever plans.
const PLUGIN: &str = "helper";

/// A plan for an application that declares one plugin and one payload file.
fn plan_with_plugin(root: &Path) -> TargetBuildPlan {
    let payload = root.join("app.exe");
    std::fs::write(&payload, b"app").expect("payload file");
    let source = root.join("helper.wasm");
    std::fs::write(&source, b"plugin").expect("plugin source");
    let (size, sha256) = hash_reader(b"app".as_slice()).expect("the payload hashes");
    let (source_size, source_sha256) =
        hash_reader(b"plugin".as_slice()).expect("the plugin hashes");
    let plugin_id = PluginId::new(PLUGIN).expect("a plugin id");
    TargetBuildPlan {
        installer: Installer {
            preset: None,
            app: App {
                id: AppId::new("com.example.embedded-plugin").expect("an app id"),
                name: NonEmptyString::new("Embedded Plugin").expect("a name"),
                version: semver::Version::parse("1.0.0").expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target: zup_core::TargetTriple::parse(HOST_TARGET).expect("a target"),
            frontend: zup_core::Frontend::Gui,
            updates: None,
            install: Install {
                scope: InstallScope::User,
                directory: InstallDirectory {
                    user: Some(
                        Template::parse("${location.user_data}/EmbeddedPlugin")
                            .expect("a directory"),
                    ),
                    machine: None,
                },
                allow_directory_override: false,
            },
            prerequisites: Vec::new(),
            components: Vec::new(),
            component_groups: Vec::new(),
            plugins: vec![PluginBinding {
                id: plugin_id.clone(),
                component: None,
                when: None,
            }],
            files: vec![FileMapping {
                source: "app.exe".to_owned(),
                destination: Template::parse("${install}").expect("a destination"),
                component: None,
                when: None,
                allow_empty: false,
            }],
            launchers: Vec::new(),
            path: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
        },
        prerequisites: Vec::new(),
        plugins: vec![ResolvedPlugin {
            id: plugin_id,
            source,
            source_relative: RelativePath::new("helper.wasm").expect("a relative path"),
            size: source_size,
            sha256: source_sha256,
        }],
        files: vec![ResolvedFile {
            source: payload,
            source_relative: RelativePath::new("app.exe").expect("a relative path"),
            destination: Template::parse("${install}").expect("a destination"),
            size,
            sha256,
            component: None,
            condition: None,
        }],
        ui_assets: Vec::new(),
        total_size: size,
        prerequisite_size: 0,
        icons: zup_core::TargetIcons::default(),
    }
}

/// The machine the host runs, which is the machine a runtime template is for.
const HOST_TARGET: &str = zup_plugin_contract::HOST_TARGET;

/// An executor that records whether it was ever asked to plan anything.
///
/// The count is in an `Arc` rather than in the executor so a caller can read it
/// after the executor has been moved into the preparation.
struct RecordingExecutor {
    target: zup_core::TargetTriple,
    planned: Arc<AtomicUsize>,
}

impl RecordingExecutor {
    fn new(target: zup_core::TargetTriple) -> (Self, Arc<AtomicUsize>) {
        let planned = Arc::new(AtomicUsize::new(0));
        (
            Self {
                target,
                planned: planned.clone(),
            },
            planned,
        )
    }
}

impl PluginExecutor for RecordingExecutor {
    fn target(&self) -> &zup_core::TargetTriple {
        &self.target
    }

    fn plan(
        &mut self,
        _binding: &zup_core::PluginBinding,
        _context: &PluginPlanningContext,
        _cancellation: &dyn CancellationQuery,
    ) -> Result<PluginResourceProposal, PluginFailure> {
        self.planned.fetch_add(1, Ordering::SeqCst);
        unreachable!("the fixture declares a plugin that is never planned in this test")
    }
}

fn committed_ledger(build: &TargetBuildPlan) -> zup_exec::InstallLedger {
    let mut ledger = zup_exec::InstallLedger::new(
        build.installer.app.id.clone(),
        build.installer.target.clone(),
        SelectedScope::User,
    );
    ledger.version = build.installer.app.version.clone();
    ledger
}

#[test]
fn an_uninstall_removes_by_ownership_and_never_loads_the_plugin_executor() {
    let root = TempDir::new().expect("temp dir");
    let build = plan_with_plugin(root.path());
    let (executor, planned) = RecordingExecutor::new(build.installer.target.clone());
    // The target cannot be lowered on a host without the Windows backend, so this
    // fixture is expected to fail - and it must fail without ever asking the
    // plugin executor anything, which is the property under test.
    let result = prepare_embedded_request(
        EmbeddedPreparation {
            request: Request::Named(LifecycleAction::Uninstall),
            scope: SelectedScope::User,
            build: &build,
            prior: Some(committed_ledger(&build)),
            state_root: root.path().join("state"),
            payload_root: root.path().to_path_buf(),
            enable: Vec::new(),
            disable: Vec::new(),
            install_directory: None,
        },
        None,
        true,
        &zup_plan::NeverCancelled,
        || Ok(executor),
    );
    assert!(
        result.is_err(),
        "the fixture target is not lowerable on this host"
    );
    assert_eq!(
        planned.load(Ordering::SeqCst),
        0,
        "an uninstall must not load the component engine at all"
    );
}

#[test]
fn a_cancelled_window_cancels_planning_before_execution_starts() {
    let root = TempDir::new().expect("temp dir");
    let build = plan_with_plugin(root.path());
    let cancel = zup_runtime::CancellationHandle::new();
    cancel.cancel();
    let (executor, _) = RecordingExecutor::new(build.installer.target.clone());
    let result = prepare_embedded_request(
        EmbeddedPreparation {
            request: Request::Apply,
            scope: SelectedScope::User,
            build: &build,
            prior: None,
            state_root: root.path().join("state"),
            payload_root: root.path().to_path_buf(),
            enable: Vec::new(),
            disable: Vec::new(),
            install_directory: None,
        },
        None,
        true,
        &RuntimeCancellationQuery(&cancel),
        || Ok(executor),
    );
    let error = match result {
        Ok(_) => panic!("cancelled planning unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("planning was cancelled"),
        "{error}"
    );
}

#[test]
fn a_repair_refuses_a_selection_it_does_not_use() {
    let root = TempDir::new().expect("temp dir");
    let build = plan_with_plugin(root.path());
    let (executor, _) = RecordingExecutor::new(build.installer.target.clone());
    let outcome = prepare_embedded_request(
        EmbeddedPreparation {
            request: Request::Named(LifecycleAction::Repair { force_files: false }),
            scope: SelectedScope::User,
            build: &build,
            prior: Some(committed_ledger(&build)),
            state_root: root.path().join("state"),
            payload_root: root.path().to_path_buf(),
            enable: vec!["docs".to_owned()],
            disable: Vec::new(),
            install_directory: None,
        },
        None,
        true,
        &zup_plan::NeverCancelled,
        || Ok(executor),
    );
    let error = match outcome {
        Ok(_) => panic!("a repair accepted a component selection"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("committed component selection"), "{error}");
}
/// A target plan with nothing in it, for the window-attach tests to fill.
fn empty_target(scope: SelectedScope, id: &str) -> zup_platform::TargetPlan {
    let target = TargetTriple::parse(zup_plugin_contract::HOST_TARGET).expect("a target");
    zup_platform::TargetPlan {
        app: App {
            id: AppId::new(id).expect("an app id"),
            name: NonEmptyString::new("Window").expect("a name"),
            version: semver::Version::parse("1.0.0").expect("a version"),
            publisher: None,
            main: None,
            description: None,
        },
        target: target.clone(),
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
        ui: None,
    }
}

/// A window with settings and one application-provided asset.
fn preset() -> UiPreset {
    UiPreset {
        name: NonEmptyString::new("aurora").expect("a name"),
        version: semver::Version::parse("1.4.2").expect("a version"),
        protocol: zup_ui_protocol::UI_PROTOCOL_VERSION,
        required_capabilities: vec!["components".to_owned()],
        settings: serde_json::json!({ "hero": "Install Acme" }),
        assets: vec![UiAsset {
            name: NonEmptyString::new("branding/logo.svg").expect("a name"),
            size: 6,
            sha256: zup_core::hash_bytes(b"<svg/>"),
        }],
    }
}

const LOGO: &[u8] = b"<svg/>";
const PRESET_EXECUTABLE: &[u8] = b"the preset executable";

/// The window an installation will present, resolved into installed content.
///
/// Attached rather than declared, because the preset executable is content: a
/// build knows which preset, and only whoever supplied the bytes can name them.
/// Each scope's content is that scope's, in that scope's directory, carried by
/// that scope's authority.
#[test]
fn a_window_is_attached_as_scope_aware_installed_content() {
    let root = TempDir::new().expect("a scratch directory");
    let app = AppId::new("com.example.window").expect("an app id");
    let version = semver::Version::parse("1.0.0").expect("a version");
    let source = |name: &str| -> miette::Result<Vec<u8>> {
        Ok(if name == zup_windows::ui_runtime::PRESET_SOURCE {
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
        let directory = zup_windows::maintenance_directory(&state_root, &app, scope, &version);
        let mut target = empty_target(scope, "com.example.window");
        attach_ui_runtime(&mut target, &directory, scope, &preset(), &source)
            .expect("the window is attached");

        let runtime = target.ui.as_ref().expect("the plan records the window");
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

        let executable = zup_windows::ui_runtime::preset_path(&directory, &runtime.executable);
        let asset = zup_windows::ui_runtime::asset_path(
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
            destinations.contains(&zup_windows::plain_path_text(&executable)),
            "the preset executable is at {}",
            executable.display()
        );
        assert!(
            destinations.contains(&zup_windows::plain_path_text(&asset)),
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
            zup_windows::ui_runtime::PRESET_SOURCE,
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

/// A payload that cannot supply the preset's bytes is refused rather than planned
/// around. An installation that committed a window it cannot open is the one
/// state this whole mechanism exists to prevent.
#[test]
fn a_window_whose_executable_cannot_be_read_is_refused() {
    let root = TempDir::new().expect("a scratch directory");
    let mut target = empty_target(SelectedScope::User, "com.example.unreadable");
    let unreadable = |_: &str| -> miette::Result<Vec<u8>> {
        Err(miette::miette!("this payload carries no preset"))
    };
    let error = attach_ui_runtime(
        &mut target,
        &root.path().join("maintenance"),
        SelectedScope::User,
        &preset(),
        &unreadable,
    )
    .expect_err("a window with no executable cannot be installed");
    assert!(error.to_string().contains("could not be read"), "{error}");
    assert!(
        target.ui.is_none(),
        "and the plan records no window rather than one it cannot honour"
    );
    assert!(target.files.is_empty(), "and no half-installed content");
}

/// An asset whose bytes are not the content the settings named is refused, for
/// the same reason: the digest is the contract between the application and the
/// preset that will read the file.
#[test]
fn an_asset_that_is_not_the_configured_content_is_refused() {
    let root = TempDir::new().expect("a scratch directory");
    let mut target = empty_target(SelectedScope::User, "com.example.mismatch");
    let source = |name: &str| -> miette::Result<Vec<u8>> {
        Ok(if name == zup_windows::ui_runtime::PRESET_SOURCE {
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
        error.to_string().contains("branding/logo.svg"),
        "and it names the asset: {error}"
    );
    assert!(target.ui.is_none(), "and records no window");
}

/// A transition's window comes out of verified acquired content, by digest.
///
/// The whole claim of making a preset first-class release content: an update
/// reads its new window out of the same verified cache every other byte came
/// from, plans it as ordinary installed files under the same state root, and
/// records the digest the release authenticated - with nothing hashed to
/// discover it and no `.zupui` anywhere in sight.
#[test]
fn a_windows_bytes_come_from_verified_acquired_content() {
    let root = TempDir::new().expect("a scratch directory");
    let (manifest, payload) = acquired_release(root.path(), zup_core::hash_bytes(LOGO));
    let mut target = empty_target(SelectedScope::User, "com.example.acquired");
    let directory = zup_windows::maintenance_directory(
        &root.path().join("state"),
        &zup_core::AppId::new("com.example.acquired").expect("a valid id"),
        SelectedScope::User,
        &target.app.version,
    );

    attach_ui_runtime(
        &mut target,
        &directory,
        SelectedScope::User,
        &preset(),
        &acquired_ui_source(&manifest, &payload),
    )
    .expect("a release that carries the window can install it");

    let recorded = target.ui.as_ref().expect("the plan records the window");
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
        zup_windows::plain_path_text(&zup_windows::ui_runtime::asset_path(
            &directory,
            "branding/logo.svg",
            &recorded.preset.assets[0].sha256,
        )),
        zup_windows::plain_path_text(&zup_windows::ui_runtime::preset_path(
            &directory,
            &recorded.executable,
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
        ["__zup_preset__.exe", "__zup_ui_asset__/branding/logo.svg",],
        "and the plan names where the bytes come from, rather than a build-machine path"
    );
}

/// Content the release does not carry is refused, with what is missing named.
///
/// The refusal matters more than the install: an update that quietly installed a
/// window without its executable would commit a generation that cannot open, and
/// the failure would surface on a later launch as a missing file rather than as
/// the release problem it is.
#[rstest::rstest]
#[case::an_executable_the_cache_does_not_hold(true)]
#[case::an_asset_the_cache_does_not_hold(false)]
fn a_window_the_release_does_not_carry_is_refused(#[case] absent_executable: bool) {
    let root = TempDir::new().expect("a scratch directory");
    // The asset the release names is the one the cache does not hold in one case
    // and does in the other, so the difference is content rather than a name.
    let named = if absent_executable {
        zup_core::hash_bytes(LOGO)
    } else {
        zup_core::hash_bytes(b"never acquired")
    };
    let (manifest, payload) = acquired_release(root.path(), named);
    let mut target = empty_target(SelectedScope::User, "com.example.absent");
    let directory = root.path().join("maintenance");

    let manifest = if absent_executable {
        // A release that names an executable the machine never acquired.
        let mut wrong = PRESET_EXECUTABLE.to_vec();
        wrong.push(b'!');
        let mut manifest = manifest;
        manifest.preset = Some(zup_artifact::Descriptor::of(
            zup_artifact::MediaType::PRESET,
            &wrong,
        ));
        manifest
    } else {
        manifest
    };

    let error = attach_ui_runtime(
        &mut target,
        &directory,
        SelectedScope::User,
        &preset(),
        &acquired_ui_source(&manifest, &payload),
    )
    .expect_err("a release that does not carry the window cannot install one");
    assert!(
        !error.to_string().is_empty(),
        "and the refusal says what is wrong: {error}"
    );
    assert!(target.ui.is_none(), "and the plan records no window");
    assert!(target.files.is_empty(), "and no half-installed content");
}

/// A release carrying one window's bytes, and a verified cache holding them.
///
/// The same shape a real acquisition leaves behind: a manifest that names the
/// content, a catalog that describes it, and a cache that has proved every blob
/// it holds. No network and no target lowering, because the claim under test is
/// about which bytes reach the plan.
fn acquired_release(
    root: &Path,
    asset_digest: zup_core::Sha256Digest,
) -> (
    zup_artifact::VariantManifest,
    zup_bundle::AcquiredPayloadSource,
) {
    let preset = zup_core::UiPreset {
        assets: vec![zup_core::UiAsset {
            name: zup_core::NonEmptyString::new("branding/logo.svg").expect("a name"),
            size: LOGO.len() as u64,
            sha256: asset_digest,
        }],
        ..preset()
    };
    let plan = zup_bundle::PortableBuildPlan {
        installer: zup_core::Installer {
            app: zup_core::App {
                id: zup_core::AppId::new("com.example.acquired").expect("a valid id"),
                name: zup_core::NonEmptyString::new("Acquired").expect("a name"),
                version: "1.0.0".parse().expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target: zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("a target"),
            frontend: zup_core::Frontend::Gui,
            preset: Some(preset.clone()),
            updates: None,
            install: zup_core::Install {
                scope: zup_core::InstallScope::User,
                directory: zup_core::InstallDirectory {
                    user: Some(zup_core::Template::parse("${install}").expect("a dir")),
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
        ui_assets: vec![zup_core::UiAsset {
            name: zup_core::NonEmptyString::new("branding/logo.svg").expect("a name"),
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
        target: zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("a target"),
        platform: zup_artifact::Platform::from_triple(
            &zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("a target"),
        ),
        frontend: zup_core::Frontend::Gui,
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
    let manifest =
        zup_artifact::VariantManifest::parse(&serde_json::to_vec(&manifest).expect("serializes"))
            .expect("a manifest this crate wrote is one it reads");

    let cache = zup_acquire::ContentCache::open(root.join("cache"), zup_acquire::CachePolicy::Auto)
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
