//! Compile the authoring model into normalized Installer IR.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use zup_core::{
    App, ComponentId, Install, InstallScope, Installer, PluginBinding, Prerequisite,
    PrerequisitePackage, PrerequisiteRequirement, ResolvedTargetConfig, TargetOverrides, Ui,
};

use crate::error::ManifestError;
use crate::model::{Manifest, Targeted, Updates};
use crate::plugin::is_valid_source;
use crate::target::{resolve_target_config, validate_target_matrix, validate_target_references};

struct ManifestView {
    app: App,
    ui: Ui,
    install: Install,
    prerequisites: Vec<Prerequisite>,
    updates: Option<Updates>,
    components: Vec<zup_core::Component>,
    component_groups: Vec<zup_core::ComponentGroup>,
    plugins: Vec<crate::Plugin>,
    files: Vec<zup_core::FileMapping>,
    launchers: Vec<zup_core::Launcher>,
    path: Vec<zup_core::PathEntry>,
    services: Vec<zup_core::Service>,
    protocols: Vec<zup_core::Protocol>,
    file_associations: Vec<zup_core::FileAssociation>,
}

impl ManifestView {
    fn for_target(manifest: &Manifest, target: &ResolvedTargetConfig) -> Self {
        let profile = &target.profile;
        Self {
            app: manifest.app.clone(),
            ui: manifest.ui.clone(),
            install: target.install.clone(),
            prerequisites: targeted_values(&manifest.prerequisites, profile),
            updates: manifest.updates.clone(),
            components: targeted_values(&manifest.components, profile),
            component_groups: targeted_values(&manifest.component_groups, profile),
            plugins: targeted_values(&manifest.plugins, profile),
            files: targeted_values(&manifest.files, profile),
            launchers: targeted_values(&manifest.launchers, profile),
            path: targeted_values(&manifest.path, profile),
            services: targeted_values(&manifest.services, profile),
            protocols: targeted_values(&manifest.protocols, profile),
            file_associations: targeted_values(&manifest.file_associations, profile),
        }
    }
}

fn targeted_values<T: Clone>(
    values: &[Targeted<T>],
    profile: &zup_core::TargetProfileId,
) -> Vec<T> {
    values
        .iter()
        .filter(|value| value.applies_to(profile))
        .map(|value| value.value.clone())
        .collect()
}

/// Compile a parsed manifest and one selected target into Installer IR.
///
/// The resolved config and caller overrides must still match the declared
/// profile. Other validation covers uniqueness, cross-references, component
/// graphs, and install-directory coverage. No filesystem access.
pub fn compile(
    manifest: &Manifest,
    target: &ResolvedTargetConfig,
    overrides: &TargetOverrides,
) -> Result<Installer, ManifestError> {
    validate_target_matrix(manifest)?;
    validate_target_references(manifest)?;
    validate_resolved_target(manifest, target, overrides)?;
    let view = ManifestView::for_target(manifest, target);

    validate_install(&view.install)?;
    validate_ui(&view.ui)?;
    if let Some(updates) = &view.updates
        && (updates.channel.is_empty()
            || updates.channel.len() > 32
            || !updates
                .channel
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || updates.root.as_os_str().is_empty())
    {
        return Err(ManifestError::Invalid {
            message: "[updates] requires a non-empty root path and a channel containing only lowercase ASCII letters, digits, and hyphens".into(),
            src: None,
            span: None,
        });
    }
    validate_components(&view.components)?;
    validate_groups(&view.component_groups, &view.components)?;
    validate_prerequisites(&view)?;
    validate_resources(&view)?;

    let plugins = view
        .plugins
        .iter()
        .map(|plugin| PluginBinding {
            id: plugin.id.clone(),
            component: plugin.component.clone(),
            when: plugin.when.clone(),
        })
        .collect();

    Ok(Installer {
        app: view.app,
        target: target.target.clone(),
        frontend: target.frontend,
        // The preset a GUI target presents is chosen from a `.zupui` the build
        // verifies, so it is not something this crate can resolve.
        preset: None,
        updates: None,
        install: view.install,
        prerequisites: view.prerequisites,
        components: view.components,
        component_groups: view.component_groups,
        plugins,
        files: view.files,
        launchers: view.launchers,
        path: view.path,
        services: view.services,
        protocols: view.protocols,
        file_associations: view.file_associations,
    })
}

/// Parse and compile one selected target without caller overrides.
///
/// Source text is attached to diagnostics.
pub fn parse_and_compile(source: &str, selector: &str) -> Result<Installer, ManifestError> {
    parse_and_compile_named(source, "zup.toml", selector)
}

/// Parse and compile one target without overrides using a diagnostic source name.
pub fn parse_and_compile_named(
    source: &str,
    name: &str,
    selector: &str,
) -> Result<Installer, ManifestError> {
    let manifest = crate::parse::parse_named(source, name)?;
    let overrides = TargetOverrides::default();
    let selected = crate::select_targets(&manifest, &[selector], &overrides)
        .map_err(|err| err.with_source_named(source, name))?;
    let target = &selected[0];
    compile(&manifest, target, &overrides).map_err(|err| err.with_source_named(source, name))
}

/// Reject a resolved config that no manifest declaration and no declared
/// override can produce.
///
/// The expected value is recomputed through the single resolution function, so
/// a caller can only smuggle a source or install-directory override by also
/// declaring it. Every field is compared, and every diagnostic names the field
/// that drifted.
fn validate_resolved_target(
    manifest: &Manifest,
    resolved: &ResolvedTargetConfig,
    overrides: &TargetOverrides,
) -> Result<(), ManifestError> {
    let invalid = |reason: String| ManifestError::InvalidResolvedTargetConfig {
        profile: resolved.profile.to_string(),
        reason,
        src: None,
        span: None,
    };
    let Some(declared) = manifest.build.targets.get(&resolved.profile) else {
        return Err(invalid(
            "profile is not declared in [build.targets]".to_owned(),
        ));
    };
    let expected = resolve_target_config(
        &resolved.profile,
        declared,
        manifest.frontend,
        &manifest.install,
        overrides,
    );
    if resolved.target != expected.target {
        return Err(invalid(format!(
            "target is `{}`, expected `{}`",
            resolved.target, expected.target
        )));
    }
    if resolved.source != expected.source {
        return Err(invalid(format!(
            "source is `{}`, expected `{}` from the declared override and profile",
            resolved.source.directory.display(),
            expected.source.directory.display()
        )));
    }
    if resolved.install.scope != expected.install.scope {
        return Err(invalid(format!(
            "install scope is `{}`, expected `{}` from the profile or common manifest",
            resolved.install.scope, expected.install.scope
        )));
    }
    if resolved.install.allow_directory_override != expected.install.allow_directory_override {
        return Err(invalid(
            "install directory-override policy does not match the profile or common manifest"
                .to_owned(),
        ));
    }
    if resolved.install.directory != expected.install.directory {
        return Err(invalid(format!(
            "install directory is `{}`, expected `{}` from the declared override and profile",
            render_install_directory(&resolved.install),
            render_install_directory(&expected.install),
        )));
    }
    if resolved.frontend != expected.frontend {
        return Err(invalid(format!(
            "frontend is `{}`, expected `{}` from the supplied overrides and declaration",
            resolved.frontend, expected.frontend
        )));
    }
    Ok(())
}

/// A readable summary of an install directory, for a resolution diagnostic.
fn render_install_directory(install: &Install) -> String {
    let user = install
        .directory
        .user
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "-".to_owned());
    let machine = install
        .directory
        .machine
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "-".to_owned());
    format!("user={user} machine={machine}")
}

/// What can be checked about `[ui]` without the preset in hand.
///
/// Whether the settings fit the preset's own schema needs the packaged schema,
/// so that is the build's check. What is checkable here is that the values
/// themselves are a shape a schema could describe: a bounded, well-named
/// document. A `.zupui` path is already refused of `..` and absolute paths by
/// the type that holds it.
fn validate_ui(ui: &Ui) -> Result<(), ManifestError> {
    let encoded = serde_json::to_vec(&ui.settings).map_err(|error| ManifestError::Invalid {
        message: format!("[ui.settings] is not a value: {error}"),
        src: None,
        span: None,
    })?;
    if encoded.len() > zup_core::MAX_PRESET_SETTINGS_BYTES {
        return Err(ManifestError::Invalid {
            message: format!(
                "[ui.settings] is {} bytes; the limit is {}",
                encoded.len(),
                zup_core::MAX_PRESET_SETTINGS_BYTES
            ),
            src: None,
            span: None,
        });
    }
    if ui.settings.keys().any(|key| key.trim().is_empty()) {
        return Err(ManifestError::Invalid {
            message: "a [ui.settings] name must not be empty".into(),
            src: None,
            span: None,
        });
    }
    Ok(())
}

fn validate_install(install: &Install) -> Result<(), ManifestError> {
    let scope = install.scope;
    if scope.allows_user() {
        let Some(user) = install.directory.user.as_ref() else {
            return Err(ManifestError::MissingInstallDirectory {
                scope,
                src: None,
                span: None,
            });
        };
        if user.contains_variable(zup_core::Variable::Install) {
            return Err(ManifestError::RecursiveInstallDirectory {
                scope: InstallScope::User,
                src: None,
                span: None,
            });
        }
    }
    if scope.allows_machine() {
        let Some(machine) = install.directory.machine.as_ref() else {
            return Err(ManifestError::MissingInstallDirectory {
                scope,
                src: None,
                span: None,
            });
        };
        if machine.contains_variable(zup_core::Variable::Install) {
            return Err(ManifestError::RecursiveInstallDirectory {
                scope: InstallScope::Machine,
                src: None,
                span: None,
            });
        }
    }
    Ok(())
}

fn validate_groups(
    groups: &[zup_core::ComponentGroup],
    components: &[zup_core::Component],
) -> Result<(), ManifestError> {
    let mut seen = BTreeSet::new();
    for group in groups {
        if !seen.insert(group.id.clone()) {
            return Err(ManifestError::Invalid {
                message: format!("component group `{}` is declared more than once", group.id),
                src: None,
                span: None,
            });
        }
    }
    for component in components {
        if let Some(group) = &component.group
            && !seen.contains(group)
        {
            return Err(ManifestError::UnknownComponent {
                id: group.to_string(),
                context: format!("component `{}` names a group", component.id),
                src: None,
                span: None,
            });
        }
    }
    for group in groups {
        if !components
            .iter()
            .any(|component| component.group.as_ref() == Some(&group.id))
        {
            return Err(ManifestError::Invalid {
                message: format!("component group `{}` has no components", group.id),
                src: None,
                span: None,
            });
        }
    }
    Ok(())
}

fn validate_components(components: &[zup_core::Component]) -> Result<(), ManifestError> {
    let mut seen = BTreeSet::new();
    for component in components {
        if !seen.insert(component.id.clone()) {
            return Err(ManifestError::DuplicateComponent {
                id: component.id.to_string(),
                src: None,
                span: None,
            });
        }
        if component.required && !component.default {
            return Err(ManifestError::RequiredComponentDisabled {
                id: component.id.to_string(),
                src: None,
                span: None,
            });
        }
    }

    let index: BTreeMap<ComponentId, &zup_core::Component> = components
        .iter()
        .map(|component| (component.id.clone(), component))
        .collect();

    for component in components {
        for required in &component.requires {
            if required == &component.id {
                return Err(ManifestError::ComponentSelfDependency {
                    id: component.id.to_string(),
                    src: None,
                    span: None,
                });
            }
            if !index.contains_key(required) {
                return Err(ManifestError::UnknownComponent {
                    id: required.to_string(),
                    context: format!("component `{}`", component.id),
                    src: None,
                    span: None,
                });
            }
        }
    }

    check_cycles(&index)?;
    Ok(())
}

fn check_cycles(index: &BTreeMap<ComponentId, &zup_core::Component>) -> Result<(), ManifestError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Visiting,
        Done,
    }

    fn visit(
        id: &ComponentId,
        index: &BTreeMap<ComponentId, &zup_core::Component>,
        state: &mut BTreeMap<ComponentId, State>,
        stack: &mut Vec<ComponentId>,
    ) -> Result<(), ManifestError> {
        match state.get(id) {
            Some(State::Done) => return Ok(()),
            Some(State::Visiting) => {
                let start = stack.iter().position(|entry| entry == id).unwrap_or(0);
                let mut path: Vec<String> =
                    stack[start..].iter().map(ToString::to_string).collect();
                path.push(id.to_string());
                return Err(ManifestError::ComponentCycle {
                    path: path.join(" → "),
                    src: None,
                    span: None,
                });
            }
            None => {}
        }

        state.insert(id.clone(), State::Visiting);
        stack.push(id.clone());

        if let Some(component) = index.get(id) {
            for required in &component.requires {
                visit(required, index, state, stack)?;
            }
        }

        stack.pop();
        state.insert(id.clone(), State::Done);
        Ok(())
    }

    let mut state = BTreeMap::new();
    let mut stack = Vec::new();
    for id in index.keys() {
        visit(id, index, &mut state, &mut stack)?;
    }
    Ok(())
}

fn validate_prerequisites(manifest: &ManifestView) -> Result<(), ManifestError> {
    let mut seen = BTreeSet::new();
    for prerequisite in &manifest.prerequisites {
        if !seen.insert(prerequisite.id.as_str().to_ascii_lowercase()) {
            return Err(ManifestError::DuplicatePrerequisite {
                id: prerequisite.id.to_string(),
                src: None,
                span: None,
            });
        }
        validate_prerequisite(prerequisite)?;
    }
    Ok(())
}

fn validate_prerequisite(prerequisite: &Prerequisite) -> Result<(), ManifestError> {
    let invalid = |reason: &str| ManifestError::InvalidPrerequisite {
        id: prerequisite.id.to_string(),
        reason: reason.to_owned(),
        src: None,
        span: None,
    };
    if prerequisite.installer.arguments.len() > zup_core::MAX_PREREQUISITE_ARGUMENTS
        || prerequisite.installer.arguments.iter().any(|argument| {
            argument.len() > zup_core::MAX_PREREQUISITE_ARGUMENT_BYTES
                || argument.contains('\0')
                || argument.contains("${")
        })
    {
        return Err(invalid(
            "installer arguments are unbounded, contain NUL, or contain templates",
        ));
    }
    let success: BTreeSet<_> = prerequisite
        .installer
        .success_exit_codes
        .iter()
        .copied()
        .collect();
    let reboot: BTreeSet<_> = prerequisite
        .installer
        .reboot_exit_codes
        .iter()
        .copied()
        .collect();
    if success.is_empty() || reboot.is_empty() || success.intersection(&reboot).next().is_some() {
        return Err(invalid(
            "success and reboot exit-code sets must be non-empty and disjoint",
        ));
    }
    match &prerequisite.package {
        PrerequisitePackage::Embedded { path, size, .. } => {
            if *size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES {
                return Err(invalid(
                    "embedded prerequisite exceeds the package size limit",
                ));
            }
            validate_filename(path.file_name()).map_err(|reason| invalid(&reason))?;
        }
        PrerequisitePackage::Remote {
            url,
            size,
            filename,
            ..
        } => {
            let parsed =
                url::Url::parse(url).map_err(|_| invalid("remote prerequisite URL is invalid"))?;
            if parsed.scheme() != "https"
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
            {
                return Err(invalid(
                    "remote prerequisite URL must be HTTPS without credentials or fragments",
                ));
            }
            if size.is_some_and(|size| size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES) {
                return Err(invalid(
                    "remote prerequisite exceeds the package size limit",
                ));
            }
            validate_filename(filename).map_err(|reason| invalid(&reason))?;
        }
    }
    if let PrerequisiteRequirement::FileVersion(file) = &prerequisite.requirement {
        if file
            .path
            .parts()
            .iter()
            .any(|part| matches!(part, zup_core::TemplatePart::Variable(_)))
        {
            return Err(invalid(
                "prerequisite file-version paths must be literal absolute paths",
            ));
        }
        if file
            .path
            .as_literal()
            .is_none_or(|value| !Path::new(value).is_absolute())
        {
            return Err(invalid("prerequisite file-version path must be absolute"));
        }
    }
    Ok(())
}

fn validate_filename(filename: &str) -> Result<(), String> {
    let valid = !filename.is_empty()
        && filename.len() <= 255
        && !filename.contains(['/', '\\', ':', '\0'])
        && filename != "."
        && filename != ".."
        && !filename.ends_with(['.', ' '])
        && !filename.chars().any(char::is_control)
        && !matches!(
            filename
                .split('.')
                .next()
                .unwrap_or("")
                .to_ascii_uppercase()
                .as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        );
    if valid {
        Ok(())
    } else {
        Err("remote prerequisite filename is not a safe Windows basename".into())
    }
}

fn validate_resources(manifest: &ManifestView) -> Result<(), ManifestError> {
    let known: BTreeSet<ComponentId> = manifest
        .components
        .iter()
        .map(|component| component.id.clone())
        .collect();

    let mut plugins = BTreeSet::new();
    for plugin in &manifest.plugins {
        if !is_valid_source(&plugin.source) {
            return Err(ManifestError::InvalidPluginSource {
                path: plugin.source.clone(),
                src: None,
                span: None,
            });
        }
        if !plugins.insert(plugin.id.as_str().to_ascii_lowercase()) {
            return Err(ManifestError::DuplicatePlugin {
                id: plugin.id.to_string(),
                src: None,
                span: None,
            });
        }
    }

    let mut services = BTreeSet::new();
    for service in &manifest.services {
        if !services.insert(service.id.clone()) {
            return Err(ManifestError::DuplicateService {
                id: service.id.to_string(),
                src: None,
                span: None,
            });
        }
    }

    // Unconditional identity collisions (no `when`) are authoring errors.
    let mut protocols = BTreeSet::new();
    for protocol in &manifest.protocols {
        if protocol.when.is_none() && !protocols.insert(protocol.scheme.clone()) {
            return Err(ManifestError::DuplicateProtocol {
                scheme: protocol.scheme.to_string(),
                src: None,
                span: None,
            });
        }
    }

    let mut file_association_ids = BTreeSet::new();
    let mut file_association_exts = BTreeSet::new();
    for file_association in &manifest.file_associations {
        if file_association.when.is_none() {
            if !file_association_ids.insert(file_association.id.clone()) {
                return Err(ManifestError::DuplicateFileAssociation {
                    id: file_association.id.to_string(),
                    src: None,
                    span: None,
                });
            }
            if !file_association_exts.insert(file_association.extension.clone()) {
                return Err(ManifestError::DuplicateExtension {
                    extension: file_association.extension.to_string(),
                    src: None,
                    span: None,
                });
            }
        }
    }

    for prerequisite in &manifest.prerequisites {
        check_ref(prerequisite.component.as_ref(), &known, "prerequisite")?;
        check_condition(prerequisite.when.as_ref(), &known, "prerequisite condition")?;
    }
    for file in &manifest.files {
        check_ref(file.component.as_ref(), &known, "file mapping")?;
        check_condition(file.when.as_ref(), &known, "condition")?;
    }
    for launcher in &manifest.launchers {
        check_ref(launcher.component.as_ref(), &known, "launcher")?;
        check_condition(launcher.when.as_ref(), &known, "condition")?;
    }
    for entry in &manifest.path {
        check_ref(entry.component.as_ref(), &known, "path entry")?;
        check_condition(entry.when.as_ref(), &known, "condition")?;
    }
    for service in &manifest.services {
        check_ref(service.component.as_ref(), &known, "service")?;
        check_condition(service.when.as_ref(), &known, "condition")?;
    }
    for protocol in &manifest.protocols {
        check_condition(protocol.when.as_ref(), &known, "condition")?;
    }
    for file_association in &manifest.file_associations {
        check_condition(file_association.when.as_ref(), &known, "condition")?;
    }
    for plugin in &manifest.plugins {
        check_ref(plugin.component.as_ref(), &known, "plugin")?;
        check_condition(plugin.when.as_ref(), &known, "plugin condition")?;
    }

    Ok(())
}

fn check_ref(
    component: Option<&ComponentId>,
    known: &BTreeSet<ComponentId>,
    context: &str,
) -> Result<(), ManifestError> {
    if let Some(id) = component
        && !known.contains(id)
    {
        return Err(ManifestError::UnknownComponent {
            id: id.to_string(),
            context: context.to_owned(),
            src: None,
            span: None,
        });
    }
    Ok(())
}

fn check_condition(
    condition: Option<&zup_core::Condition>,
    known: &BTreeSet<ComponentId>,
    context: &str,
) -> Result<(), ManifestError> {
    let Some(condition) = condition else {
        return Ok(());
    };
    for id in condition.referenced_components() {
        if !known.contains(&id) {
            return Err(ManifestError::UnknownComponent {
                id: id.to_string(),
                context: context.to_owned(),
                src: None,
                span: None,
            });
        }
    }
    Ok(())
}
