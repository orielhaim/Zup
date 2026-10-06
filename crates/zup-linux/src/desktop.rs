//! Deterministic freedesktop `.desktop` rendering.
//!
//! Zup renders desktop entries itself rather than depending on a desktop-entry
//! serialization crate. The format is small, and owning the renderer is what
//! makes the output deterministic byte-for-byte: fixed field order, one
//! escaping rule, validated field codes, no environment-specific keys.
//!
//! Authority is the freedesktop Desktop Entry Specification. Only fields Zup
//! understands are emitted; deprecated keys are never written.

use thiserror::Error;

/// Why a desktop entry could not be rendered.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DesktopRenderError {
    #[error("desktop entry name is empty")]
    EmptyName,
    #[error("desktop entry name `{name}` cannot be represented: {reason}")]
    InvalidName { name: String, reason: String },
    #[error("executable path is empty")]
    EmptyExecutable,
    #[error("executable or argument cannot be represented in `Exec=`: {reason}")]
    InvalidExec { reason: String },
    #[error("MIME type `{value}` is not a valid handler identifier")]
    InvalidMimeType { value: String },
    #[error("working directory is empty")]
    EmptyWorkingDirectory,
}

/// One file/URL dispatch code Zup can lower into `Exec=`.
///
/// The portable model expresses at most "open one file" or "open one URI",
/// so these are the only codes the renderer accepts. Anything else in author
/// input is literal text and is escaped, never passed through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldCode {
    /// One local file.
    SingleFile,
    /// One URI (used for protocol handlers and for files only where the
    /// portable model promises URI delivery, which it currently does not).
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

/// One `Exec=` argument: either literal text or a Zup-chosen field code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecArgument {
    Literal(String),
    Field(FieldCode),
}

/// A rendered `Exec=` value: an executable plus ordered arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCommand {
    pub executable: String,
    pub arguments: Vec<ExecArgument>,
}

impl ExecCommand {
    /// Render `Exec=` without any shell: the Desktop Entry quoting grammar,
    /// not shell syntax. Never `sh -c`, never shell quoting.
    pub fn render(&self) -> Result<String, DesktopRenderError> {
        if self.executable.is_empty() {
            return Err(DesktopRenderError::EmptyExecutable);
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

/// Quote one `Exec=` word per the Desktop Entry Specification.
///
/// A literal `%` is escaped as `%%` so author text can never inject a field
/// code. Control characters cannot be represented and are refused.
fn quote_word(text: &str) -> Result<String, DesktopRenderError> {
    let invalid = |reason: &str| DesktopRenderError::InvalidExec {
        reason: reason.to_owned(),
    };
    if text.chars().any(char::is_control) {
        return Err(invalid("control characters cannot appear in `Exec=`"));
    }
    let escaped = text.replace('%', "%%");
    if escaped.is_empty() {
        return Ok("\"\"".to_owned());
    }
    let safe = escaped
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '+' | '@' | ','));
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

/// Escape a `Name=`/`Path=` string value: backslash, newline, tab,
/// carriage return. Spaces are literal, which the specification permits.
fn escape_string(text: &str) -> Result<String, DesktopRenderError> {
    let invalid = |reason: &str| DesktopRenderError::InvalidName {
        name: text.to_owned(),
        reason: reason.to_owned(),
    };
    if text.is_empty() {
        return Err(DesktopRenderError::EmptyName);
    }
    if text.chars().any(|c| c == '\n' || c == '\r') {
        return Err(invalid("a name cannot contain a line break"));
    }
    // Tab is escapable per the specification and is escaped below; every
    // other control character has no representation and is refused.
    if text.chars().any(|c| c.is_control() && c != '\t') {
        return Err(invalid("a name cannot contain control characters"));
    }
    Ok(text
        .replace('\\', "\\\\")
        .replace('\t', "\\t"))
}

/// One application or handler desktop entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// Display name. Never used for identity.
    pub name: String,
    pub exec: ExecCommand,
    /// Theme icon name for `Icon=`, absent when the application ships no icon.
    pub icon: Option<String>,
    /// Absolute working directory, only when the portable launcher names one.
    pub working_directory: Option<String>,
    /// Handler MIME types (`x-scheme-handler/*`, shared-mime types).
    /// A visible launcher carries none; handler entries carry at least one.
    pub mime_types: Vec<String>,
    /// Hidden handler entry: usable for dispatch, not shown as a launcher.
    pub hidden: bool,
}

impl DesktopEntry {
    /// Render the complete file, deterministically byte-for-byte.
    ///
    /// Field order is fixed. `MimeType=` always ends with `;`, as required.
    /// `Terminal=` is omitted: the portable model carries no per-launcher
    /// terminal flag and guessing from the executable is forbidden, so the
    /// specification default (not run in a terminal) applies.
    pub fn render(&self) -> Result<String, DesktopRenderError> {
        let name = escape_string(&self.name)?;
        let exec = self.exec.render()?;
        if let Some(icon) = &self.icon {
            validate_icon_name(icon)?;
        }
        if let Some(directory) = &self.working_directory {
            if directory.is_empty() {
                return Err(DesktopRenderError::EmptyWorkingDirectory);
            }
            if directory.chars().any(char::is_control) {
                return Err(DesktopRenderError::InvalidName {
                    name: directory.clone(),
                    reason: "a working directory cannot contain control characters".into(),
                });
            }
        }
        for mime in &self.mime_types {
            validate_mime_type(mime)?;
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

fn validate_icon_name(icon: &str) -> Result<(), DesktopRenderError> {
    if icon.is_empty() || icon.contains(['/', '\0']) || icon.chars().any(char::is_control) {
        return Err(DesktopRenderError::InvalidName {
            name: icon.to_owned(),
            reason: "an icon name is a theme name, not a path".into(),
        });
    }
    Ok(())
}

fn validate_mime_type(value: &str) -> Result<(), DesktopRenderError> {
    let valid = value
        .split_once('/')
        .is_some_and(|(major, minor)| !major.is_empty() && !minor.is_empty())
        && !value.contains([';', ' ', '\0'])
        && !value.chars().any(char::is_control);
    if valid {
        Ok(())
    } else {
        Err(DesktopRenderError::InvalidMimeType {
            value: value.to_owned(),
        })
    }
}

/// Derive a stable desktop/icon resource stem from an application id.
///
/// Identity, never display text: renaming the application in v2 keeps every
/// filename. Characters outside `[A-Za-z0-9_.-]` are replaced; when that
/// replacement is lossy a hash suffix keeps two different ids from sharing
/// one filename.
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
        assert!(rendered.contains(
            "MimeType=x-scheme-handler/acme;application/x-com-example-tool-foo;\n"
        ), "{rendered}");
        assert!(rendered.contains("NoDisplay=true\n"), "{rendered}");
    }

    /// `Exec=` is not shell syntax: spaces quote, shell metacharacters are
    /// quoted rather than escaped, and `%` never smuggles a field code.
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
        assert!(rendered.contains("Name=My Tool\\t2000 \\\\ Pro\n"), "{rendered}");
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
}
