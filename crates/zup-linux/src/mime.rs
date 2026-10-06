//! Deterministic Shared MIME-info package rendering.
//!
//! Authority is the freedesktop Shared MIME-info Specification. Zup generates
//! one application-owned package per application under
//! `$XDG_DATA_HOME/mime/packages/`, holding only the custom types the
//! manifest declares: a MIME identifier, the file globs, and the author's
//! description. No magic-byte detection is invented, no inheritance is
//! guessed, and no standard IANA type is claimed for application-local types.
//!
//! The generated database (`mime.cache`, `globs2`, …) is derived state owned
//! by no application; refreshing it is a typed integration action, not a file
//! Zup owns.

use thiserror::Error;

/// Why a MIME package could not be rendered.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MimeRenderError {
    #[error("MIME comment for `{mime}` cannot be represented: {reason}")]
    InvalidComment { mime: String, reason: String },
    #[error("file extension `{extension}` is not a valid glob extension")]
    InvalidExtension { extension: String },
    #[error("MIME type `{value}` is not a valid custom type name")]
    InvalidType { value: String },
}

/// One custom file type in the application package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MimeTypeDefinition {
    /// Full type name, e.g. `application/x-com-example-tool-foo`.
    pub mime_type: String,
    /// Author description, used as the `<comment>`.
    pub comment: String,
    /// File extension including the leading dot, e.g. `.foo`.
    pub extension: String,
}

/// Derive the stable MIME identifier for one file association.
///
/// Deterministic from application and association identity — never from
/// display text — so v2 keeps the same identifier. Custom types live under
/// `application/x-` and never claim a standard IANA type. When sanitization
/// is lossy a hash suffix keeps two identities from sharing one type.
pub fn mime_type_for(app_id: &str, extension: &str) -> String {
    let app = sanitize(&app_id.to_lowercase());
    let ext = sanitize(&extension.to_lowercase().trim_start_matches('.'));
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
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if matches!(c, '-' | '_' | '.') {
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

/// Validate a derived or supplied custom type name.
pub fn validate_mime_type(value: &str) -> Result<(), MimeRenderError> {
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
        Err(MimeRenderError::InvalidType {
            value: value.to_owned(),
        })
    }
}

/// Render the complete package XML, deterministically byte-for-byte.
///
/// Types are sorted by name; the output is fixed apart from the definitions.
pub fn render_package(definitions: &[MimeTypeDefinition]) -> Result<String, MimeRenderError> {
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
            return Err(MimeRenderError::InvalidExtension {
                extension: definition.extension.clone(),
            });
        }
        let comment =
            escape_xml(&definition.comment).map_err(|reason| MimeRenderError::InvalidComment {
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
