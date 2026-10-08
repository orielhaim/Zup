use std::path::Path;

use zup_core::{LauncherLocation, ResourceKey, SelectedScope, TargetTriple};
use zup_plan::InstallPlan;
use zup_platform::TargetPath;

use crate::error::PlanError;
use crate::locations::user_data_home;

pub const GENERATED_PREFIX: &str = "__zup_generated__";

#[derive(Debug, Clone)]
pub struct GeneratedFile {
    pub key: ResourceKey,
    pub source_relative: String,
    pub destination: TargetPath,
    pub bytes: Vec<u8>,
    pub size: u64,
    pub sha256: zup_core::Sha256Digest,
}

#[derive(Debug, Clone, Default)]
pub struct IntegrationOutput {
    pub files: Vec<GeneratedFile>,

    pub needs_mime_refresh: bool,

    pub needs_desktop_refresh: bool,
}

pub fn lower_integration(plan: &InstallPlan) -> Result<IntegrationOutput, PlanError> {
    refuse_unsupported(plan)?;
    if plan.scope != SelectedScope::User {
        if !plan.launchers.is_empty()
            || !plan.protocols.is_empty()
            || !plan.file_associations.is_empty()
        {
            return Err(PlanError::IntegrationUnsupported(
                "launchers, URI protocols, and file associations are not supported in machine scope: machine desktop integration is deferred past this phase".into(),
            ));
        }
        return Ok(IntegrationOutput::default());
    }
    if plan.target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        return Err(PlanError::IntegrationUnsupported(format!(
            "target `{}` is not a Linux target",
            plan.target.as_str()
        )));
    }
    if plan.launchers.is_empty() && plan.protocols.is_empty() && plan.file_associations.is_empty() {
        return Ok(IntegrationOutput::default());
    }
    lower_integration_in(plan, &user_data_home()?)
}

pub fn lower_integration_in(
    plan: &InstallPlan,
    data_home: &std::path::Path,
) -> Result<IntegrationOutput, PlanError> {
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
        (a.target.to_string(), a.name.to_string()).cmp(&(b.target.to_string(), b.name.to_string()))
    });
    for (index, launcher) in menu_launchers.iter().enumerate() {
        let file_name = if index == 0 {
            format!("{stem}.desktop")
        } else {
            format!("{stem}-launcher-{}.desktop", index + 1)
        };
        let destination = xdg_target(
            &plan.target,
            &data_home.join("applications").join(&file_name),
        )?;
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
            working_directory: working_directory.as_ref().map(host_text).transpose()?,
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
        let mut schemes: Vec<String> = plan
            .protocols
            .iter()
            .map(|p| p.scheme.to_string())
            .collect();
        schemes.sort();
        schemes.dedup();
        let mut arguments = Vec::new();
        let mut executable: Option<String> = None;
        for protocol in &plan.protocols {
            let target = resolve_install_template(&plan.target, &protocol.executable)?;
            let text = host_text(&target)?;
            if executable.as_ref().is_some_and(|first| *first != text) {
                return Err(PlanError::IntegrationUnsupported(
                    "protocol handlers for different executables need distinct desktop entries, which this phase does not lower".into(),
                ));
            }
            executable = Some(text);
            arguments.push(protocol_args(protocol)?);
        }
        let Some(executable) = executable else {
            return Err(PlanError::IntegrationUnsupported(
                "no protocol handler".into(),
            ));
        };
        let first = arguments.remove(0);
        for rest in &arguments {
            if *rest != first {
                return Err(PlanError::IntegrationUnsupported(
                    "protocol handlers with different arguments need distinct desktop entries, which this phase does not lower".into(),
                ));
            }
        }
        let destination = xdg_target(
            &plan.target,
            &data_home
                .join("applications")
                .join(format!("{stem}-uri.desktop")),
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
        sorted.sort_by_key(|a| a.extension.to_string());
        for association in sorted {
            let target = resolve_install_template(&plan.target, &association.executable)?;
            let text = host_text(&target)?;
            if executable.as_ref().is_some_and(|first| *first != text) {
                return Err(PlanError::IntegrationUnsupported(
                    "file associations for different executables need distinct desktop entries, which this phase does not lower".into(),
                ));
            }
            executable = Some(text);
            let mime = mime_type_for(plan.app.id.as_str(), association.extension.as_str());
            mime_types.push(mime.clone());
            definitions.push(MimeTypeDefinition {
                mime_type: mime,
                comment: association
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("{} document", association.extension.as_str())),
                extension: association.extension.to_string(),
            });
        }
        let package = render_package(&definitions)?;
        let package_destination = xdg_target(
            &plan.target,
            &data_home
                .join("mime")
                .join("packages")
                .join(format!("{stem}.xml")),
        )?;
        output.needs_mime_refresh = true;
        output.files.push(generated(
            &format!("{GENERATED_PREFIX}/mime/{stem}.xml"),
            package_destination,
            package.into_bytes(),
        )?);
        let destination = xdg_target(
            &plan.target,
            &data_home
                .join("applications")
                .join(format!("{stem}-files.desktop")),
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

pub fn generated_map(
    plan: &InstallPlan,
) -> Result<std::collections::BTreeMap<String, Vec<u8>>, PlanError> {
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

pub fn save_generated(
    state_root: &std::path::Path,
    app_id: &zup_core::AppId,
    scope: SelectedScope,
    generated: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<(), PlanError> {
    let path = generated_store_path(state_root, app_id, scope);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| PlanError::InvalidTargetPath {
            path: parent.display().to_string(),
            reason: error.to_string(),
        })?;
    }

    let text: std::collections::BTreeMap<&str, &str> = generated
        .iter()
        .map(|(name, bytes)| {
            std::str::from_utf8(bytes)
                .map(|text| (name.as_str(), text))
                .map_err(|_| PlanError::InvalidTargetPath {
                    path: name.clone(),
                    reason: "generated content is not UTF-8".into(),
                })
        })
        .collect::<Result<_, _>>()?;
    let serialized = serde_json::to_vec(&text).map_err(|error| PlanError::InvalidTargetPath {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    std::fs::write(&path, &serialized).map_err(|error| PlanError::InvalidTargetPath {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    Ok(())
}

pub fn load_generated(
    state_root: &std::path::Path,
    app_id: &zup_core::AppId,
    scope: SelectedScope,
) -> Result<std::collections::BTreeMap<String, Vec<u8>>, PlanError> {
    let path = generated_store_path(state_root, app_id, scope);
    let serialized = std::fs::read(&path).map_err(|error| PlanError::InvalidTargetPath {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    let text: std::collections::BTreeMap<String, String> = serde_json::from_slice(&serialized)
        .map_err(|error| PlanError::InvalidTargetPath {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
    Ok(text
        .into_iter()
        .map(|(name, content)| (name, content.into_bytes()))
        .collect())
}

fn protocol_args(protocol: &zup_plan::PlannedProtocol) -> Result<Vec<ExecArgument>, PlanError> {
    if zup_core::uri_placeholder_count(&protocol.args) != 1 {
        return Err(PlanError::IntegrationUnsupported(format!(
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

fn refuse_unsupported(plan: &InstallPlan) -> Result<(), PlanError> {
    if plan
        .launchers
        .iter()
        .any(|l| l.location == LauncherLocation::Desktop)
    {
        return Err(PlanError::IntegrationUnsupported(
            "a `desktop` launcher is not supported on Linux in this phase: a file in \
             `$XDG_DATA_HOME/applications` makes an application discoverable, which the \
             `menu` location provides, but placing a trusted icon on the user's desktop \
             has no desktop-neutral implementation"
                .into(),
        ));
    }
    if !plan.path_entries.is_empty() {
        return Err(PlanError::IntegrationUnsupported(format!(
            "{} PATH entry resource{} {} not supported on Linux in this phase: a directory-level \
             PATH mutation is not command exposure, and Linux command exposure without editing \
             shell configuration needs a portable `command` semantic this phase does not add",
            plan.path_entries.len(),
            if plan.path_entries.len() == 1 {
                ""
            } else {
                "s"
            },
            if plan.path_entries.len() == 1 {
                "is"
            } else {
                "are"
            },
        )));
    }

    if plan.scope != SelectedScope::Machine && !plan.services.is_empty() {
        return Err(PlanError::IntegrationUnsupported(format!(
            "{} service resource{} {} not supported on Linux in this phase",
            plan.services.len(),
            if plan.services.len() == 1 { "" } else { "s" },
            if plan.services.len() == 1 {
                "is"
            } else {
                "are"
            },
        )));
    }
    if !plan.prerequisites.is_empty() {
        return Err(PlanError::IntegrationUnsupported(format!(
            "{} package-manager prerequisite resource{} {} not supported on Linux in this phase",
            plan.prerequisites.len(),
            if plan.prerequisites.len() == 1 {
                ""
            } else {
                "s"
            },
            if plan.prerequisites.len() == 1 {
                "is"
            } else {
                "are"
            },
        )));
    }
    Ok(())
}

fn resolve_install_template(
    target: &TargetTriple,
    template: &zup_core::Template,
) -> Result<TargetPath, PlanError> {
    use zup_platform::resolve_template_path;
    let path = resolve_template_path(
        template,
        target,
        &crate::locations::LinuxInstallLocationResolver::default(),
        SelectedScope::User,
    )
    .map_err(|error| PlanError::InvalidTargetPath {
        path: template.to_string(),
        reason: error.to_string(),
    })?;
    crate::lowering::to_host_path(&path).map_err(|error| PlanError::InvalidTargetPath {
        path: path.to_string(),
        reason: error.to_string(),
    })?;
    Ok(path)
}

fn xdg_target(target: &TargetTriple, path: &Path) -> Result<TargetPath, PlanError> {
    let text = path.to_string_lossy().into_owned();
    TargetPath::new(target.clone(), &text).map_err(|error| PlanError::InvalidTargetPath {
        path: text,
        reason: error.to_string(),
    })
}

fn host_text(path: &TargetPath) -> Result<String, PlanError> {
    crate::lowering::to_host_path(path)
        .map(|host| host.to_string_lossy().into_owned())
        .map_err(|error| PlanError::InvalidTargetPath {
            path: path.to_string(),
            reason: error.to_string(),
        })
}

fn generated(
    source_relative: &str,
    destination: TargetPath,
    bytes: Vec<u8>,
) -> Result<GeneratedFile, PlanError> {
    let (size, sha256) =
        zup_core::hash_reader(bytes.as_slice()).map_err(|_| PlanError::InvalidTargetPath {
            path: destination.to_string(),
            reason: "generated content does not hash".into(),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldCode {
    SingleFile,

    SingleUri,
}

impl FieldCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::SingleFile => "%f",
            Self::SingleUri => "%u",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecArgument {
    Literal(String),
    Field(FieldCode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCommand {
    pub executable: String,
    pub arguments: Vec<ExecArgument>,
}

impl ExecCommand {
    pub fn render(&self) -> Result<String, PlanError> {
        if self.executable.is_empty() {
            return Err(PlanError::EmptyExecutable);
        }
        let mut parts = vec![quote_word(&self.executable)?];
        for argument in &self.arguments {
            match argument {
                ExecArgument::Field(code) => parts.push(code.as_str().to_owned()),
                ExecArgument::Literal(text) => parts.push(quote_word(text)?),
            }
        }
        Ok(parts.join(" "))
    }
}

fn quote_word(text: &str) -> Result<String, PlanError> {
    let invalid = |reason: &str| PlanError::InvalidExec {
        reason: reason.to_owned(),
    };
    if text.chars().any(char::is_control) {
        return Err(invalid("control characters cannot appear in `Exec=`"));
    }
    let escaped = text.replace('%', "%%");
    if escaped.is_empty() {
        return Ok("\"\"".to_owned());
    }
    let safe = escaped.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '+' | '@' | ',')
    });
    if safe {
        return Ok(escaped);
    }
    let mut quoted = String::with_capacity(escaped.len() + 2);
    quoted.push('"');
    for c in escaped.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    Ok(quoted)
}

fn escape_string(text: &str) -> Result<String, PlanError> {
    let invalid = |reason: &str| PlanError::InvalidName {
        name: text.to_owned(),
        reason: reason.to_owned(),
    };
    if text.is_empty() {
        return Err(PlanError::EmptyName);
    }
    if text.chars().any(|c| c == '\n' || c == '\r') {
        return Err(invalid("a name cannot contain a line break"));
    }

    if text.chars().any(|c| c.is_control() && c != '\t') {
        return Err(invalid("a name cannot contain control characters"));
    }
    Ok(text.replace('\\', "\\\\").replace('\t', "\\t"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    pub name: String,
    pub exec: ExecCommand,

    pub icon: Option<String>,

    pub working_directory: Option<String>,

    pub mime_types: Vec<String>,

    pub hidden: bool,
}

impl DesktopEntry {
    pub fn render(&self) -> Result<String, PlanError> {
        let name = escape_string(&self.name)?;
        let exec = self.exec.render()?;
        if let Some(icon) = &self.icon {
            validate_icon_name(icon)?;
        }
        if let Some(directory) = &self.working_directory {
            if directory.is_empty() {
                return Err(PlanError::EmptyWorkingDirectory);
            }
            if directory.chars().any(char::is_control) {
                return Err(PlanError::InvalidName {
                    name: directory.clone(),
                    reason: "a working directory cannot contain control characters".into(),
                });
            }
        }
        for mime in &self.mime_types {
            validate_handler_type(mime)?;
        }
        let mut out = String::from("[Desktop Entry]\n");
        out.push_str("Type=Application\n");
        out.push_str(&format!("Name={name}\n"));
        out.push_str(&format!("Exec={exec}\n"));
        if let Some(icon) = &self.icon {
            out.push_str(&format!("Icon={icon}\n"));
        }
        if let Some(directory) = &self.working_directory {
            out.push_str(&format!("Path={}\n", escape_path(directory)));
        }
        if !self.mime_types.is_empty() {
            let mut joined = self.mime_types.join(";");
            joined.push(';');
            out.push_str(&format!("MimeType={joined}\n"));
        }
        if self.hidden {
            out.push_str("NoDisplay=true\n");
        }
        Ok(out)
    }
}

fn escape_path(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\t', "\\t")
}

fn validate_icon_name(icon: &str) -> Result<(), PlanError> {
    if icon.is_empty() || icon.contains(['/', '\0']) || icon.chars().any(char::is_control) {
        return Err(PlanError::InvalidName {
            name: icon.to_owned(),
            reason: "an icon name is a theme name, not a path".into(),
        });
    }
    Ok(())
}

fn validate_handler_type(value: &str) -> Result<(), PlanError> {
    let valid = value
        .split_once('/')
        .is_some_and(|(major, minor)| !major.is_empty() && !minor.is_empty())
        && !value.contains([';', ' ', '\0'])
        && !value.chars().any(char::is_control);
    if valid {
        Ok(())
    } else {
        Err(PlanError::InvalidMimeType {
            value: value.to_owned(),
        })
    }
}

pub fn stable_stem(app_id: &str) -> String {
    let sanitized: String = app_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized == app_id && !sanitized.is_empty() {
        return sanitized;
    }
    let (size, digest) = zup_core::hash_reader(app_id.as_bytes()).expect("an id hashes");
    debug_assert_eq!(size, app_id.len() as u64);
    format!("{sanitized}-{}", &digest.to_hex()[..8])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MimeTypeDefinition {
    pub mime_type: String,

    pub comment: String,

    pub extension: String,
}

pub fn mime_type_for(app_id: &str, extension: &str) -> String {
    let app = sanitize(&app_id.to_lowercase());
    let ext = sanitize(extension.to_lowercase().trim_start_matches('.'));
    let base = format!("application/x-{app}-{ext}");
    if app == app_id.to_lowercase() && ext == extension.to_lowercase().trim_start_matches('.') {
        return base;
    }
    let (size, digest) = zup_core::hash_reader(format!("{app_id}|{extension}").as_bytes())
        .expect("an identity hashes");
    debug_assert_eq!(size, app_id.len() as u64 + 1 + extension.len() as u64);
    format!("{base}-{}", &digest.to_hex()[..8])
}

fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_dash = true;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '_' || c == '.');
    if trimmed.is_empty() {
        "zup".to_owned()
    } else {
        trimmed.to_owned()
    }
}

pub fn validate_mime_type(value: &str) -> Result<(), PlanError> {
    let valid = value.split_once('/').is_some_and(|(major, minor)| {
        !major.is_empty()
            && !minor.is_empty()
            && major.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '!' | '#' | '$' | '&' | '^' | '_' | '.' | '+' | '-')
            })
            && minor.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '!' | '#' | '$' | '&' | '^' | '_' | '.' | '+' | '-')
            })
    });
    if valid {
        Ok(())
    } else {
        Err(PlanError::Type {
            value: value.to_owned(),
        })
    }
}

pub fn render_package(definitions: &[MimeTypeDefinition]) -> Result<String, PlanError> {
    let mut sorted = definitions.to_vec();
    sorted.sort_by(|left, right| left.mime_type.cmp(&right.mime_type));
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<mime-info xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\">\n",
    );
    for definition in &sorted {
        validate_mime_type(&definition.mime_type)?;
        if !definition.extension.starts_with('.')
            || definition.extension.len() < 2
            || definition.extension.contains(['/', '\\', '\0', '*', '?'])
            || definition.extension.chars().any(char::is_control)
        {
            return Err(PlanError::Extension {
                extension: definition.extension.clone(),
            });
        }
        let comment = escape_xml(&definition.comment).map_err(|reason| PlanError::Comment {
            mime: definition.mime_type.clone(),
            reason: reason.to_owned(),
        })?;
        let glob = format!(
            "*.{}",
            escape_xml(&definition.extension[1..]).expect("extension is escapable")
        );
        out.push_str(&format!(
            "  <mime-type type=\"{}\">\n    <comment>{}</comment>\n    <glob pattern=\"{}\"/>\n  </mime-type>\n",
            definition.mime_type, comment, glob
        ));
    }
    out.push_str("</mime-info>\n");
    Ok(out)
}

fn escape_xml(text: &str) -> Result<String, &'static str> {
    if text.chars().any(char::is_control) {
        return Err("a comment cannot contain control characters");
    }
    Ok(text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> ExecCommand {
        ExecCommand {
            executable: "/home/u/.local/lib/zup/apps/tool/tool".into(),
            arguments: Vec::new(),
        }
    }

    #[test]
    fn a_visible_launcher_renders_deterministically() {
        let entry = DesktopEntry {
            name: "Tool".into(),
            exec: command(),
            icon: Some("com.example.tool".into()),
            working_directory: None,
            mime_types: Vec::new(),
            hidden: false,
        };
        let first = entry.render().expect("renders");
        let second = entry.render().expect("renders");
        assert_eq!(first, second);
        assert_eq!(
            first,
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Tool\n\
             Exec=/home/u/.local/lib/zup/apps/tool/tool\n\
             Icon=com.example.tool\n"
        );
    }

    #[test]
    fn a_handler_entry_is_hidden_and_lists_mime_types() {
        let entry = DesktopEntry {
            name: "Tool".into(),
            exec: ExecCommand {
                executable: "/opt/tool".into(),
                arguments: vec![ExecArgument::Field(FieldCode::SingleUri)],
            },
            icon: None,
            working_directory: None,
            mime_types: vec![
                "x-scheme-handler/acme".into(),
                "application/x-com-example-tool-foo".into(),
            ],
            hidden: true,
        };
        let rendered = entry.render().expect("renders");
        assert!(rendered.contains("Exec=/opt/tool %u\n"), "{rendered}");
        assert!(
            rendered
                .contains("MimeType=x-scheme-handler/acme;application/x-com-example-tool-foo;\n"),
            "{rendered}"
        );
        assert!(rendered.contains("NoDisplay=true\n"), "{rendered}");
    }

    #[rstest::rstest]
    #[case::plain("/opt/tool", &["--help"][..], "/opt/tool --help")]
    #[case::spaces("/opt/My Tool/tool", &[][..], "\"/opt/My Tool/tool\"")]
    #[case::arg_with_spaces("/opt/tool", &["a b"][..], "/opt/tool \"a b\"")]
    #[case::dollar("/opt/tool", &["$HOME"][..], "/opt/tool \"\\$HOME\"")]
    #[case::backtick("/opt/tool", &["`id`"][..], "/opt/tool \"\\`id\\`\"")]
    #[case::backslash("/opt/tool", &["a\\b"][..], "/opt/tool \"a\\\\b\"")]
    #[case::quote("/opt/tool", &["say \"hi\""][..], "/opt/tool \"say \\\"hi\\\"\"")]
    #[case::percent("/opt/tool", &["100%"][..], "/opt/tool \"100%%\"")]
    #[case::percent_code("/opt/tool", &["%f"][..], "/opt/tool \"%%f\"")]
    #[case::unicode_exe("/opt/tööl", &[][..], "\"/opt/tööl\"")]
    #[case::unicode_arg("/opt/tool", &["héllo"][..], "/opt/tool \"h\u{e9}llo\"")]
    #[case::empty_arg("/opt/tool", &[""][..], "/opt/tool \"\"")]
    fn exec_quoting(#[case] exe: &str, #[case] args: &[&str], #[case] expected: &str) {
        let rendered = ExecCommand {
            executable: exe.into(),
            arguments: args
                .iter()
                .map(|a| ExecArgument::Literal((*a).to_owned()))
                .collect(),
        }
        .render()
        .expect("renders");
        assert_eq!(rendered, expected, "for {args:?}");
    }

    #[test]
    fn control_characters_are_refused_not_escaped() {
        for text in ["a\nb", "a\tb", "a\rb", "a\x07b"] {
            assert!(
                ExecCommand {
                    executable: "/opt/tool".into(),
                    arguments: vec![ExecArgument::Literal(text.into())],
                }
                .render()
                .is_err(),
                "{text:?} must be refused"
            );
        }
    }

    #[test]
    fn names_escape_backslash_and_tab_but_keep_spaces() {
        let entry = DesktopEntry {
            name: "My Tool\t2000 \\ Pro".into(),
            exec: command(),
            icon: None,
            working_directory: None,
            mime_types: Vec::new(),
            hidden: false,
        };
        let rendered = entry.render().expect("renders");
        assert!(
            rendered.contains("Name=My Tool\\t2000 \\\\ Pro\n"),
            "{rendered}"
        );
    }

    #[test]
    fn line_breaks_in_names_are_refused() {
        let entry = DesktopEntry {
            name: "Tool\nEvil=1".into(),
            exec: command(),
            icon: None,
            working_directory: None,
            mime_types: Vec::new(),
            hidden: false,
        };
        assert!(entry.render().is_err(), "injection via newline is refused");
    }

    #[test]
    fn mime_types_must_be_typed_and_terminated() {
        for bad in ["", "noslash", "/minor", "major/", "a;b", "a b/c"] {
            let entry = DesktopEntry {
                name: "Tool".into(),
                exec: command(),
                icon: None,
                working_directory: None,
                mime_types: vec![bad.into()],
                hidden: true,
            };
            assert!(entry.render().is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn icon_names_are_names_not_paths() {
        let entry = DesktopEntry {
            name: "Tool".into(),
            exec: command(),
            icon: Some("/opt/icon.png".into()),
            working_directory: None,
            mime_types: Vec::new(),
            hidden: false,
        };
        assert!(entry.render().is_err(), "absolute icon paths are refused");
    }

    #[rstest::rstest]
    #[case::plain_id("com.example.tool", "com.example.tool")]
    #[case::dashes("Acme-Tool_2.0", "Acme-Tool_2.0")]
    fn stable_stems_keep_clean_ids(#[case] id: &str, #[case] expected: &str) {
        assert_eq!(stable_stem(id), expected);
    }

    #[test]
    fn lossy_ids_gain_collision_resistance() {
        let first = stable_stem("com:example/tool");
        let second = stable_stem("com;example?tool");
        assert_ne!(first, second, "different ids must not share a stem");
        assert!(first.starts_with("com_example_tool-"), "{first}");
    }

    #[test]
    fn derivation_is_stable_and_never_claims_iana_types() {
        let first = mime_type_for("com.example.tool", ".foo");
        assert_eq!(first, mime_type_for("com.example.tool", ".foo"));
        assert_eq!(first, "application/x-com.example.tool-foo");
        assert!(mime_type_for("com.example.tool", ".BAR").starts_with("application/x-"));
    }

    #[test]
    fn lossy_identities_do_not_share_a_type() {
        let first = mime_type_for("com:example", ".foo");
        let second = mime_type_for("com;example", ".foo");
        assert_ne!(first, second, "sanitization collisions gain a hash suffix");
    }

    #[test]
    fn a_package_renders_deterministically() {
        let definitions = vec![
            MimeTypeDefinition {
                mime_type: "application/x-com-example-tool-bar".into(),
                comment: "Bar document".into(),
                extension: ".bar".into(),
            },
            MimeTypeDefinition {
                mime_type: "application/x-com-example-tool-foo".into(),
                comment: "Foo & \"Friends\" <doc>".into(),
                extension: ".foo".into(),
            },
        ];
        let first = render_package(&definitions).expect("renders");
        let reversed = render_package(&definitions.iter().rev().cloned().collect::<Vec<_>>())
            .expect("renders");
        assert_eq!(first, reversed, "order of input does not matter");
        let expected = concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<mime-info xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\">\n",
            "  <mime-type type=\"application/x-com-example-tool-bar\">\n",
            "    <comment>Bar document</comment>\n",
            "    <glob pattern=\"*.bar\"/>\n",
            "  </mime-type>\n",
            "  <mime-type type=\"application/x-com-example-tool-foo\">\n",
            "    <comment>Foo &amp; &quot;Friends&quot; &lt;doc&gt;</comment>\n",
            "    <glob pattern=\"*.foo\"/>\n",
            "  </mime-type>\n",
            "</mime-info>\n",
        );
        assert_eq!(first, expected);
    }

    #[test]
    fn control_characters_are_refused() {
        let definitions = vec![MimeTypeDefinition {
            mime_type: "application/x-com-example-tool-foo".into(),
            comment: "bad\x07comment".into(),
            extension: ".foo".into(),
        }];
        assert!(render_package(&definitions).is_err());
    }
}
