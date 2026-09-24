//! Compile the authoring model into normalized Installer IR.

use std::collections::{BTreeMap, BTreeSet};

use zup_core::{ActionId, ComponentId, Install, InstallScope, Installer};

use crate::error::ManifestError;
use crate::model::Manifest;

/// Compile a parsed manifest into platform-independent Installer IR.
///
/// Performs semantic validation only: uniqueness, cross-references, component
/// graph checks, and install-directory coverage. No filesystem access.
pub fn compile(manifest: Manifest) -> Result<Installer, ManifestError> {
    validate_install(&manifest.install)?;
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
    validate_resources(&manifest)?;

    Ok(Installer {
        app: manifest.app,
        updates: None,
        install: manifest.install,
        components: manifest.components,
        files: manifest.files,
        shortcuts: manifest.shortcuts,
        path: manifest.path,
        services: manifest.services,
        protocols: manifest.protocols,
        file_types: manifest.file_types,
        actions: manifest.actions,
    })
}

/// Parse and compile in one step, attaching source text to diagnostics.
pub fn parse_and_compile(source: &str) -> Result<Installer, ManifestError> {
    let manifest = crate::parse::parse(source)?;
    compile(manifest).map_err(|err| err.with_source(source))
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

fn validate_resources(manifest: &Manifest) -> Result<(), ManifestError> {
    let known: BTreeSet<ComponentId> = manifest
        .components
        .iter()
        .map(|component| component.id.clone())
        .collect();

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

    let mut actions = BTreeSet::<ActionId>::new();
    for action in &manifest.actions {
        if !actions.insert(action.id.clone()) {
            return Err(ManifestError::DuplicateAction {
                id: action.id.to_string(),
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

    for file in &manifest.files {
        check_ref(file.component.as_ref(), &known, "file mapping")?;
        check_condition(file.when.as_ref(), &known)?;
    }
    for shortcut in &manifest.shortcuts {
        check_ref(shortcut.component.as_ref(), &known, "shortcut")?;
        check_condition(shortcut.when.as_ref(), &known)?;
    }
    for entry in &manifest.path {
        check_ref(entry.component.as_ref(), &known, "path entry")?;
        check_condition(entry.when.as_ref(), &known)?;
    }
    for service in &manifest.services {
        check_ref(service.component.as_ref(), &known, "service")?;
        check_condition(service.when.as_ref(), &known)?;
    }
    for protocol in &manifest.protocols {
        check_condition(protocol.when.as_ref(), &known)?;
    }
    for file_type in &manifest.file_types {
        check_condition(file_type.when.as_ref(), &known)?;
    }
    for action in &manifest.actions {
        check_ref(action.component.as_ref(), &known, "action")?;
        check_condition(action.when.as_ref(), &known)?;
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
) -> Result<(), ManifestError> {
    let Some(condition) = condition else {
        return Ok(());
    };
    for id in condition.referenced_components() {
        if !known.contains(&id) {
            return Err(ManifestError::UnknownComponent {
                id: id.to_string(),
                context: "condition".to_owned(),
                src: None,
                span: None,
            });
        }
    }
    Ok(())
}
