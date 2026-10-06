//! Lowering portable integration intent into Linux-native desired state.
//!
//! The portable model expresses intent: a launcher, a protocol scheme, a file
//! extension. Linux lowers that intent into native resources: `.desktop`
//! entries under `$XDG_DATA_HOME/applications`, a Shared MIME-info package
//! under `$XDG_DATA_HOME/mime/packages/`, and cache refreshes of the derived
//! databases those sources feed.
//!
//! Lowering produces generated files, not backend abstractions. A generated
//! file is an ordinary transaction file whose bytes are rendered here and
//! served from memory at install time: snapshot, delta, receipts, ledger,
//! repair, uninstall, rollback, and recovery all treat it as what it is. The
//! shared caches those files feed (`mime.cache`, `mimeinfo.cache`) are never
//! owned; refreshing them is a typed integration operation.
//!
//! Deliberately unsupported, with diagnostics rather than false translations:
//!
//! - `LauncherLocation::Desktop`: a file in `applications/` is discoverability,
//!   not a physical desktop icon, and modern environments disagree on how a
//!   desktop icon becomes trusted. No desktop-neutral implementation exists.
//! - `PathEntry`: a directory-level `PATH` mutation is not a command exposure,
//!   and shell rc files are not edited casually. There is no honest mapping.
//! - `mimeapps.list`: the portable association promises capability ("can
//!   open"), never default ownership, and that is exactly what `MimeType=`
//!   plus Shared MIME-info expresses. User preferences are never touched.

use std::path::PathBuf;

use zup_core::{LauncherLocation, ResourceKey, SelectedScope, TargetTriple};
use zup_plan::InstallPlan;
use zup_platform::TargetPath;

use crate::desktop::{DesktopEntry, DesktopRenderError, ExecArgument, ExecCommand, FieldCode, stable_stem};
use crate::locations::user_data_home;
use crate::mime::{MimeRenderError, MimeTypeDefinition, mime_type_for, render_package};

/// Marker prefix for generated integration payloads.
///
/// Served from memory at install time (like the maintenance copy), never from
/// the package: the bytes name absolute install paths that only exist on the
/// target machine.
pub const GENERATED_PREFIX: &str = "__zup_generated__";

/// Why portable intent has no honest Linux lowering.
#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error("{0}")]
    Unsupported(String),
    #[error("cannot resolve the XDG data home: {0}")]
    DataHome(#[from] crate::locations::LinuxLocationError),
    #[error("desktop entry cannot be rendered: {0}")]
    Render(#[from] DesktopRenderError),
    #[error("MIME package cannot be rendered: {0}")]
    Mime(#[from] MimeRenderError),
    #[error("target path `{path}` is not a valid Linux path: {reason}")]
    InvalidPath { path: String, reason: String },
}

/// One generated integration file: destination plus deterministic bytes.
#[derive(Debug, Clone)]
pub struct GeneratedFile {
    pub key: ResourceKey,
    pub source_relative: String,
    pub destination: TargetPath,
    pub bytes: Vec<u8>,
    pub size: u64,
    pub sha256: zup_core::Sha256Digest,
}

/// Lowered Linux integration: generated files plus which derived databases
/// those files feed.
#[derive(Debug, Clone, Default)]
pub struct IntegrationOutput {
    pub files: Vec<GeneratedFile>,
    /// A MIME package source changed: `update-mime-database` must run.
    pub needs_mime_refresh: bool,
    /// A desktop entry changed: `update-desktop-database` must run.
    pub needs_desktop_refresh: bool,
}

/// Lower portable launchers, protocols, and file associations.
///
/// Total over well-formed plans: every supported resource becomes generated
/// files, and anything without an honest mapping is refused with the reason.
pub fn lower_integration(plan: &InstallPlan) -> Result<IntegrationOutput, IntegrationError> {
    refuse_unsupported(plan)?;
    if plan.scope != SelectedScope::User {
        return Err(IntegrationError::Unsupported(
            "machine scope needs a privilege mechanism this phase does not have".into(),
        ));
    }
    if plan.target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        return Err(IntegrationError::Unsupported(format!(
            "target `{}` is not a Linux target",
            plan.target.as_str()
        )));
    }
    if plan.launchers.is_empty() && plan.protocols.is_empty() && plan.file_associations.is_empty()
    {
        return Ok(IntegrationOutput::default());
    }
    lower_integration_in(plan, &user_data_home()?)
}

/// The pure half of [`lower_integration`]: the policy with the data home
/// already read, so tests never mutate the process environment.
pub fn lower_integration_in(
    plan: &InstallPlan,
    data_home: &std::path::Path,
) -> Result<IntegrationOutput, IntegrationError> {
    let data_home = data_home.to_path_buf();
    let stem = stable_stem(plan.app.id.as_str());
    let icon = stable_stem(plan.app.id.as_str());
    let mut output = IntegrationOutput::default();

    let mut menu_launchers: Vec<_> = plan
        .launchers
        .iter()
        .filter(|launcher| launcher.location == LauncherLocation::Menu)
        .collect();
    menu_launchers.sort_by(|a, b| {
        (a.target.to_string(), a.name.to_string())
            .cmp(&(b.target.to_string(), b.name.to_string()))
    });
    for (index, launcher) in menu_launchers.iter().enumerate() {
        let file_name = if index == 0 {
            format!("{stem}.desktop")
        } else {
            format!("{stem}-launcher-{}.desktop", index + 1)
        };
        let destination =
            xdg_target(&plan.target, &data_home.join("applications").join(&file_name))?;
        let target = resolve_install_template(&plan.target, &launcher.target)?;
        let working_directory = launcher
            .working_directory
            .as_ref()
            .map(|template| resolve_install_template(&plan.target, template))
            .transpose()?;
        let entry = DesktopEntry {
            name: launcher.name.to_string(),
            exec: ExecCommand {
                executable: host_text(&target)?,
                arguments: launcher
                    .arguments
                    .iter()
                    .map(|argument| ExecArgument::Literal(argument.clone()))
                    .collect(),
            },
            icon: Some(icon.clone()),
            working_directory: working_directory
                .as_ref()
                .map(host_text)
                .transpose()?,
            mime_types: Vec::new(),
            hidden: false,
        };
        output.needs_desktop_refresh = true;
        output.files.push(generated(
            &format!("{GENERATED_PREFIX}/applications/{file_name}"),
            destination,
            entry.render()?.into_bytes(),
        )?);
    }

    if !plan.protocols.is_empty() {
        let mut schemes: Vec<String> = plan.protocols.iter().map(|p| p.scheme.to_string()).collect();
        schemes.sort();
        schemes.dedup();
        let mut arguments = Vec::new();
        let mut executable: Option<String> = None;
        for protocol in &plan.protocols {
            let target = resolve_install_template(&plan.target, &protocol.executable)?;
            let text = host_text(&target)?;
            if executable.as_ref().is_some_and(|first| *first != text) {
                return Err(IntegrationError::Unsupported(
                    "protocol handlers for different executables need distinct desktop entries, which this phase does not lower".into(),
                ));
            }
            executable = Some(text);
            arguments.push(protocol_args(protocol)?);
        }
        let Some(executable) = executable else {
            return Err(IntegrationError::Unsupported("no protocol handler".into()));
        };
        let first = arguments.remove(0);
        for rest in &arguments {
            if *rest != first {
                return Err(IntegrationError::Unsupported(
                    "protocol handlers with different arguments need distinct desktop entries, which this phase does not lower".into(),
                ));
            }
        }
        let destination = xdg_target(
            &plan.target,
            &data_home.join("applications").join(format!("{stem}-uri.desktop")),
        )?;
        let entry = DesktopEntry {
            name: plan.app.name.to_string(),
            exec: ExecCommand {
                executable,
                arguments: first,
            },
            icon: Some(icon.clone()),
            working_directory: None,
            mime_types: schemes
                .iter()
                .map(|scheme| format!("x-scheme-handler/{scheme}"))
                .collect(),
            hidden: true,
        };
        output.needs_desktop_refresh = true;
        output.files.push(generated(
            &format!("{GENERATED_PREFIX}/applications/{stem}-uri.desktop"),
            destination,
            entry.render()?.into_bytes(),
        )?);
    }

    if !plan.file_associations.is_empty() {
        let mut definitions = Vec::new();
        let mut mime_types = Vec::new();
        let mut executable: Option<String> = None;
        let mut sorted: Vec<_> = plan.file_associations.iter().collect();
        sorted.sort_by(|a, b| a.extension.to_string().cmp(&b.extension.to_string()));
        for association in sorted {
            let target = resolve_install_template(&plan.target, &association.executable)?;
            let text = host_text(&target)?;
            if executable.as_ref().is_some_and(|first| *first != text) {
                return Err(IntegrationError::Unsupported(
                    "file associations for different executables need distinct desktop entries, which this phase does not lower".into(),
                ));
            }
            executable = Some(text);
            let mime = mime_type_for(plan.app.id.as_str(), association.extension.as_str());
            mime_types.push(mime.clone());
            definitions.push(MimeTypeDefinition {
                mime_type: mime,
                comment: association.description.clone().unwrap_or_else(|| {
                    format!("{} document", association.extension.as_str())
                }),
                extension: association.extension.to_string(),
            });
        }
        let package = render_package(&definitions)?;
        let package_destination = xdg_target(
            &plan.target,
            &data_home.join("mime").join("packages").join(format!("{stem}.xml")),
        )?;
        output.needs_mime_refresh = true;
        output.files.push(generated(
            &format!("{GENERATED_PREFIX}/mime/{stem}.xml"),
            package_destination,
            package.into_bytes(),
        )?);
        let destination = xdg_target(
            &plan.target,
            &data_home.join("applications").join(format!("{stem}-files.desktop")),
        )?;
        let entry = DesktopEntry {
            name: plan.app.name.to_string(),
            exec: ExecCommand {
                executable: executable.expect("an association names an executable"),
                arguments: vec![ExecArgument::Field(FieldCode::SingleFile)],
            },
            icon: Some(icon.clone()),
            working_directory: None,
            mime_types,
            hidden: true,
        };
        output.needs_desktop_refresh = true;
        output.files.push(generated(
            &format!("{GENERATED_PREFIX}/applications/{stem}-files.desktop"),
            destination,
            entry.render()?.into_bytes(),
        )?);
    }

    Ok(output)
}

/// Render every generated file's bytes, keyed by payload source name.
///
/// Called by the runner to serve generated content from memory. Deterministic:
/// the same manifest always renders the same bytes, which is what lets a
/// recovery run serve what the interrupted run planned.
pub fn generated_map(
    plan: &InstallPlan,
) -> Result<std::collections::BTreeMap<String, Vec<u8>>, IntegrationError> {
    let output = lower_integration(plan)?;
    Ok(output
        .files
        .into_iter()
        .map(|file| (file.source_relative, file.bytes))
        .collect())
}

fn generated_store_path(
    state_root: &std::path::Path,
    app_id: &zup_core::AppId,
    scope: SelectedScope,
) -> std::path::PathBuf {
    let (size, digest) = zup_core::hash_reader(app_id.as_str().as_bytes()).expect("an id hashes");
    debug_assert_eq!(size, app_id.as_str().len() as u64);
    let scope_token = match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    };
    state_root
        .join("generated")
        .join(format!("{}-{scope_token}.json", digest.to_hex()))
}

/// Persist generated bytes where a recovery run can always read them.
///
/// Written before the transaction begins and keyed by application and scope,
/// never by transaction: a newer run only starts after the previous journal
/// is resolved, so the store always describes the journal under replay.
pub fn save_generated(
    state_root: &std::path::Path,
    app_id: &zup_core::AppId,
    scope: SelectedScope,
    generated: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<(), IntegrationError> {
    let path = generated_store_path(state_root, app_id, scope);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| IntegrationError::InvalidPath {
            path: parent.display().to_string(),
            reason: error.to_string(),
        })?;
    }
    // Generated renderers emit text; a non-UTF-8 byte here means the renderer
    // changed under this function, and refusing loudly beats storing bytes
    // the loader cannot return.
    let text: std::collections::BTreeMap<&str, &str> = generated
        .iter()
        .map(|(name, bytes)| {
            std::str::from_utf8(bytes)
                .map(|text| (name.as_str(), text))
                .map_err(|_| IntegrationError::InvalidPath {
                    path: name.clone(),
                    reason: "generated content is not UTF-8".into(),
                })
        })
        .collect::<Result<_, _>>()?;
    let serialized = serde_json::to_vec(&text).map_err(|error| IntegrationError::InvalidPath {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    std::fs::write(&path, &serialized).map_err(|error| IntegrationError::InvalidPath {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    Ok(())
}

/// Load generated bytes persisted by [`save_generated`].
pub fn load_generated(
    state_root: &std::path::Path,
    app_id: &zup_core::AppId,
    scope: SelectedScope,
) -> Result<std::collections::BTreeMap<String, Vec<u8>>, IntegrationError> {
    let path = generated_store_path(state_root, app_id, scope);
    let serialized = std::fs::read(&path).map_err(|error| IntegrationError::InvalidPath {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    let text: std::collections::BTreeMap<String, String> =
        serde_json::from_slice(&serialized).map_err(|error| IntegrationError::InvalidPath {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
    Ok(text
        .into_iter()
        .map(|(name, content)| (name, content.into_bytes()))
        .collect())
}
///
/// Convert portable protocol arguments into an `Exec=` argument list.
///
/// The portable vector is verbatim author text plus one `%1` URI placeholder.
/// Exactly one placeholder becomes `%u`; any other arrangement cannot
/// faithfully receive one URI and is refused rather than guessed.
fn protocol_args(protocol: &zup_plan::PlannedProtocol) -> Result<Vec<ExecArgument>, IntegrationError> {
    if zup_core::uri_placeholder_count(&protocol.args) != 1 {
        return Err(IntegrationError::Unsupported(format!(
            "protocol handler arguments must carry exactly one `%1` URI placeholder (found {}): Linux delivers the URI through `%u` and any other shape cannot be represented faithfully",
            zup_core::uri_placeholder_count(&protocol.args)
        )));
    }
    Ok(protocol
        .args
        .iter()
        .map(|argument| {
            if argument == "%1" {
                ExecArgument::Field(FieldCode::SingleUri)
            } else {
                ExecArgument::Literal(argument.clone())
            }
        })
        .collect())
}

fn refuse_unsupported(plan: &InstallPlan) -> Result<(), IntegrationError> {
    if plan.launchers.iter().any(|l| l.location == LauncherLocation::Desktop) {
        return Err(IntegrationError::Unsupported(
            "a `desktop` launcher is not supported on Linux in this phase: a file in \
             `$XDG_DATA_HOME/applications` makes an application discoverable, which the \
             `menu` location provides, but placing a trusted icon on the user's desktop \
             has no desktop-neutral implementation"
                .into(),
        ));
    }
    if !plan.path_entries.is_empty() {
        return Err(IntegrationError::Unsupported(format!(
            "{} PATH entry resource{} {} not supported on Linux in this phase: a directory-level \
             PATH mutation is not command exposure, and Linux command exposure without editing \
             shell configuration needs a portable `command` semantic this phase does not add",
            plan.path_entries.len(),
            if plan.path_entries.len() == 1 { "" } else { "s" },
            if plan.path_entries.len() == 1 { "is" } else { "are" },
        )));
    }
    if !plan.services.is_empty() {
        return Err(IntegrationError::Unsupported(format!(
            "{} service resource{} {} not supported on Linux in this phase",
            plan.services.len(),
            if plan.services.len() == 1 { "" } else { "s" },
            if plan.services.len() == 1 { "is" } else { "are" },
        )));
    }
    if !plan.prerequisites.is_empty() {
        return Err(IntegrationError::Unsupported(format!(
            "{} package-manager prerequisite resource{} {} not supported on Linux in this phase",
            plan.prerequisites.len(),
            if plan.prerequisites.len() == 1 { "" } else { "s" },
            if plan.prerequisites.len() == 1 { "is" } else { "are" },
        )));
    }
    Ok(())
}

fn resolve_install_template(
    target: &TargetTriple,
    template: &zup_core::Template,
) -> Result<TargetPath, IntegrationError> {
    use zup_platform::resolve_template_path;
    let path = resolve_template_path(
        template,
        target,
        &crate::locations::LinuxInstallLocationResolver,
        SelectedScope::User,
    )
    .map_err(|error| IntegrationError::InvalidPath {
        path: template.to_string(),
        reason: error.to_string(),
    })?;
    crate::lowering::to_host_path(&path).map_err(|error| IntegrationError::InvalidPath {
        path: path.to_string(),
        reason: error.to_string(),
    })?;
    Ok(path)
}

fn xdg_target(target: &TargetTriple, path: &PathBuf) -> Result<TargetPath, IntegrationError> {
    let text = path.to_string_lossy().into_owned();
    TargetPath::new(target.clone(), &text).map_err(|error| IntegrationError::InvalidPath {
        path: text,
        reason: error.to_string(),
    })
}

fn host_text(path: &TargetPath) -> Result<String, IntegrationError> {
    crate::lowering::to_host_path(path)
        .map(|host| host.to_string_lossy().into_owned())
        .map_err(|error| IntegrationError::InvalidPath {
            path: path.to_string(),
            reason: error.to_string(),
        })
}

fn generated(
    source_relative: &str,
    destination: TargetPath,
    bytes: Vec<u8>,
) -> Result<GeneratedFile, IntegrationError> {
    let (size, sha256) = zup_core::hash_reader(bytes.as_slice()).map_err(|_| {
        IntegrationError::InvalidPath {
            path: destination.to_string(),
            reason: "generated content does not hash".into(),
        }
    })?;
    Ok(GeneratedFile {
        key: ResourceKey::File {
            destination: destination.to_string(),
        },
        source_relative: source_relative.to_owned(),
        destination,
        bytes,
        size,
        sha256,
    })
}
