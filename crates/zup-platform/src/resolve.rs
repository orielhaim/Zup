use thiserror::Error;
use zup_core::{Template, TemplatePart, Variable};

use crate::install_locations::{InstallLocationError, InstallLocationResolver};
use crate::target_path::{TargetPath, TargetPathError};

#[derive(Debug, Error)]
pub enum TemplateResolveError {
    #[error(transparent)]
    InstallLocation(#[from] InstallLocationError),

    #[error(transparent)]
    InvalidPath(#[from] TargetPathError),

    #[error("template variable `${variable}` cannot be resolved on the target")]
    UnresolvedVariable { variable: Variable },

    /// A substituted value carried a `${` sequence that no parse had seen, so
    /// it came from substitution text rather than from the template itself.
    #[error("substituted text contains an unresolved `${{` sequence: `{text}`")]
    UnresolvedLiteral { text: String },

    /// The install location resolver answered with a path for another target.
    #[error("install location resolved to `{path}`, which is not on target `{target}`")]
    ResolverTargetMismatch { path: String, target: String },

    #[error("invalid path segment `{segment}` in template")]
    InvalidSegment { segment: String },
}

pub fn resolve_template_path<R: InstallLocationResolver + ?Sized>(
    template: &Template,
    target: &zup_core::TargetTriple,
    resolver: &R,
    scope: zup_core::SelectedScope,
) -> Result<TargetPath, TemplateResolveError> {
    let mut output: Option<TargetPath> = None;

    for part in template.parts() {
        match part {
            TemplatePart::Variable(Variable::Location(location)) => {
                let location_path = resolver.resolve(*location, scope, target)?;
                output = Some(match output {
                    None => location_path,
                    Some(existing) => append_target_path(&existing, &location_path)?,
                });
            }
            TemplatePart::Variable(variable) => {
                return Err(TemplateResolveError::UnresolvedVariable {
                    variable: *variable,
                });
            }
            TemplatePart::Literal(text) => {
                output = Some(match output {
                    None => TargetPath::new(target, text)?,
                    Some(existing) => append_literal(&existing, target, text)?,
                });
            }
        }
    }

    Ok(output.ok_or(TargetPathError::Empty)?)
}

fn append_literal(
    existing: &TargetPath,
    target: &zup_core::TargetTriple,
    text: &str,
) -> Result<TargetPath, TemplateResolveError> {
    if text.contains("${") {
        return Err(TemplateResolveError::UnresolvedLiteral {
            text: text.to_owned(),
        });
    }
    let separator = if target.operating_system() == zup_core::TargetOperatingSystem::Windows {
        '\\'
    } else {
        '/'
    };
    let mut output = existing.clone();
    let is_windows = target.operating_system() == zup_core::TargetOperatingSystem::Windows;
    for segment in
        text.split(|character| character == separator || (is_windows && character == '/'))
    {
        if segment.is_empty() {
            continue;
        }
        if segment == "." || segment == ".." {
            return Err(TemplateResolveError::InvalidSegment {
                segment: segment.to_owned(),
            });
        }
        output = output.join(segment)?;
    }
    Ok(output)
}

fn append_target_path(
    existing: &TargetPath,
    addition: &TargetPath,
) -> Result<TargetPath, TemplateResolveError> {
    // A resolver is told which target to answer for; answering for another one
    // is a broken resolver rather than a bad template.
    if existing.target() != addition.target() {
        return Err(TemplateResolveError::ResolverTargetMismatch {
            path: addition.as_str().to_owned(),
            target: addition.target().to_string(),
        });
    }
    let separator =
        if existing.target().operating_system() == zup_core::TargetOperatingSystem::Windows {
            '\\'
        } else {
            '/'
        };
    let root_len = addition.root_len();
    let suffix = addition.as_str().get(root_len..).unwrap_or_default();
    let mut output = existing.clone();
    for segment in suffix.split(separator) {
        if segment.is_empty() {
            continue;
        }
        output = output.join(segment)?;
    }
    Ok(output)
}
