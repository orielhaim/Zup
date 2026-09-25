//! Compile the authoring model into normalized Installer IR.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use zup_core::{
    ComponentId, Install, InstallScope, Installer, PluginBinding, Prerequisite,
    PrerequisiteDetector, PrerequisiteInstallerKind, PrerequisitePackage, UiBranding,
};

use crate::error::ManifestError;
use crate::model::Manifest;
use crate::plugin::is_valid_source;

/// Compile a parsed manifest into platform-independent Installer IR.
///
/// Performs semantic validation only: uniqueness, cross-references, component
/// graph checks, and install-directory coverage. No filesystem access.
pub fn compile(manifest: Manifest) -> Result<Installer, ManifestError> {
    validate_install(&manifest.install)?;
    validate_ui(manifest.ui.as_ref())?;
    if let Some(updates) = &manifest.updates
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
    validate_components(&manifest.components)?;
    validate_prerequisites(&manifest)?;
    validate_resources(&manifest)?;

    let Manifest {
        app,
        frontend,
        ui,
        install,
        prerequisites,
        components,
        files,
        shortcuts,
        path,
        services,
        protocols,
        file_types,
        plugins,
        ..
    } = manifest;
    let plugins = plugins
        .into_iter()
        .map(|plugin| PluginBinding {
            id: plugin.id,
            component: plugin.component,
            when: plugin.when,
        })
        .collect();

    Ok(Installer {
        app,
        frontend,
        ui,
        updates: None,
        install,
        prerequisites,
        components,
        plugins,
        files,
        shortcuts,
        path,
        services,
        protocols,
        file_types,
    })
}

/// Parse and compile in one step, attaching source text to diagnostics.
pub fn parse_and_compile(source: &str) -> Result<Installer, ManifestError> {
    parse_and_compile_named(source, "zup.toml")
}

pub fn parse_and_compile_named(source: &str, name: &str) -> Result<Installer, ManifestError> {
    let manifest = crate::parse::parse_named(source, name)?;
    compile(manifest).map_err(|err| err.with_source_named(source, name))
}

fn validate_ui(ui: Option<&UiBranding>) -> Result<(), ManifestError> {
    let Some(accent) = ui.and_then(|ui| ui.accent.as_deref()) else {
        return Ok(());
    };
    let valid = accent.strip_prefix('#').is_some_and(|value| {
        value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    if valid {
        Ok(())
    } else {
        Err(ManifestError::InvalidUiAccent {
            src: None,
            span: None,
        })
    }
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

fn validate_prerequisites(manifest: &Manifest) -> Result<(), ManifestError> {
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
    if prerequisite.installer.kind == PrerequisiteInstallerKind::Msi
        && !matches!(
            prerequisite.detector,
            PrerequisiteDetector::MsiProduct { .. }
        )
    {
        return Err(invalid("MSI installers require an msi_product detector"));
    }
    match &prerequisite.detector {
        PrerequisiteDetector::RegistryValue { key, value, .. } => {
            if key.is_empty() || key.contains('\0') || value.is_empty() || value.contains('\0') {
                return Err(invalid(
                    "registry detector key and value must be non-empty and NUL-free",
                ));
            }
        }
        PrerequisiteDetector::FileVersion { path, .. } => {
            if path
                .parts()
                .iter()
                .any(|part| matches!(part, zup_core::TemplatePart::Variable(_)))
            {
                return Err(invalid(
                    "prerequisite file-version paths must be literal absolute paths",
                ));
            }
            if path
                .as_literal()
                .is_none_or(|value| !Path::new(value).is_absolute())
            {
                return Err(invalid("prerequisite file-version path must be absolute"));
            }
        }
        PrerequisiteDetector::MsiProduct { product_code, .. } => {
            if product_code.is_empty()
                || product_code.len() > 256
                || product_code.contains('\0')
                || product_code.contains(['/', '\\'])
            {
                return Err(invalid("MSI product code is invalid"));
            }
        }
        PrerequisiteDetector::VisualCppV14 { .. }
        | PrerequisiteDetector::DotNetRuntime { .. }
        | PrerequisiteDetector::WebView2Evergreen { .. } => {}
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

fn validate_resources(manifest: &Manifest) -> Result<(), ManifestError> {
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

    let mut file_type_ids = BTreeSet::new();
    let mut file_type_exts = BTreeSet::new();
    for file_type in &manifest.file_types {
        if file_type.when.is_none() {
            if !file_type_ids.insert(file_type.id.clone()) {
                return Err(ManifestError::DuplicateFileType {
                    id: file_type.id.to_string(),
                    src: None,
                    span: None,
                });
            }
            if !file_type_exts.insert(file_type.extension.clone()) {
                return Err(ManifestError::DuplicateExtension {
                    extension: file_type.extension.to_string(),
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
    for shortcut in &manifest.shortcuts {
        check_ref(shortcut.component.as_ref(), &known, "shortcut")?;
        check_condition(shortcut.when.as_ref(), &known, "condition")?;
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
    for file_type in &manifest.file_types {
        check_condition(file_type.when.as_ref(), &known, "condition")?;
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
