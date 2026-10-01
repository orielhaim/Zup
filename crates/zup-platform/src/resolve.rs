use thiserror::Error;
use zup_core::{Template, TemplatePart, Variable};

use crate::install_locations::{InstallLocationError, InstallLocationResolver};
use crate::target_path::{TargetPath, TargetPathError, suffix_components};

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
                let resolved = resolver.resolve(*location, scope, target)?;
                output = Some(match output {
                    None => resolved,
                    Some(existing) => {
                        if existing.target() != resolved.target() {
                            return Err(TemplateResolveError::ResolverTargetMismatch {
                                path: resolved.as_str().to_owned(),
                                target: resolved.target().to_string(),
                            });
                        }
                        existing.extend_with(&resolved)?
                    }
                });
            }
            TemplatePart::Variable(variable) => {
                return Err(TemplateResolveError::UnresolvedVariable {
                    variable: *variable,
                });
            }
            TemplatePart::Literal(text) => {
                if text.contains("${") {
                    return Err(TemplateResolveError::UnresolvedLiteral {
                        text: text.to_owned(),
                    });
                }
                output = Some(match output {
                    None => TargetPath::new(target, text)?,
                    Some(existing) => {
                        existing.join_segments(suffix_components(target, text).map_err(
                            |segment| TemplateResolveError::InvalidSegment {
                                segment: segment.to_owned(),
                            },
                        )?)?
                    }
                });
            }
        }
    }

    Ok(output.ok_or(TargetPathError::Empty)?)
}
