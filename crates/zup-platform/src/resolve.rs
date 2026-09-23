//! Resolve structural templates into concrete target paths.

use std::path::PathBuf;
use thiserror::Error;
use zup_core::{Template, TemplatePart, Variable};

use crate::known_folders::{KnownFolderError, KnownFolderResolver, known_folder_for_variable};
use crate::target_path::{TargetPath, TargetPathError};

/// Errors produced while resolving a template to a target path.
#[derive(Debug, Error)]
pub enum TemplateResolveError {
    /// A known folder failed to resolve.
    #[error(transparent)]
    KnownFolder(#[from] KnownFolderError),

    /// The template produced an invalid target path.
    #[error(transparent)]
    InvalidPath(#[from] TargetPathError),

    /// A non-known-folder variable was still present after static substitution.
    #[error("template variable `${variable}` cannot be resolved on the target")]
    UnresolvedVariable { variable: Variable },

    /// A literal path segment was invalid.
    #[error("invalid path segment `{segment}` in template")]
    InvalidSegment { segment: String },
}

/// Resolve a structural template into an absolute [`TargetPath`].
///
/// Builds the path deliberately from parts — never by string concatenation of
/// a single giant Windows path.
pub fn resolve_template_path<R: KnownFolderResolver + ?Sized>(
    template: &Template,
    resolver: &R,
    scope: zup_core::SelectedScope,
) -> Result<TargetPath, TemplateResolveError> {
    let mut out = PathBuf::new();

    for part in template.parts() {
        match part {
            TemplatePart::Variable(variable) => {
                let Some(folder) = known_folder_for_variable(*variable) else {
                    return Err(TemplateResolveError::UnresolvedVariable {
                        variable: *variable,
                    });
                };
                let resolved = resolver.resolve(folder, scope)?;
                if out.as_os_str().is_empty() {
                    out = resolved;
                } else {
                    for component in resolved.components() {
                        out.push(component);
                    }
                }
            }
            TemplatePart::Literal(text) => {
                let normalized = text.replace('\\', "/");
                if out.as_os_str().is_empty() && std::path::Path::new(&normalized).is_absolute() {
                    if normalized.split('/').any(|segment| segment == "..") {
                        return Err(TemplateResolveError::InvalidSegment {
                            segment: "..".into(),
                        });
                    }
                    out = PathBuf::from(normalized);
                    continue;
                }
                for segment in normalized.split('/') {
                    if segment.is_empty() || segment == "." {
                        continue;
                    }
                    if segment == ".." {
                        return Err(TemplateResolveError::InvalidSegment {
                            segment: segment.to_owned(),
                        });
                    }
                    if segment.contains("${") {
                        return Err(TemplateResolveError::UnresolvedVariable {
                            variable: Variable::Install,
                        });
                    }
                    out.push(segment);
                }
            }
        }
    }

    Ok(TargetPath::new(out)?)
}
