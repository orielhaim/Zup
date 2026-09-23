//! Glob pattern compilation and static-root extraction.

use globset::GlobMatcher;

use crate::error::BuildError;

/// A compiled `[[files]]` pattern and its static directory root.
#[derive(Debug, Clone)]
pub struct FilePattern {
    /// Original pattern text from the manifest.
    pub pattern: String,
    /// Compiled matcher over `/`-separated source-relative paths.
    pub matcher: GlobMatcher,
    /// Non-pattern directory prefix that is stripped before appending to destination.
    /// `None` means the entire source-relative path is preserved.
    pub static_root: Option<String>,
}

impl FilePattern {
    /// Compile a glob and extract its static root.
    pub fn compile(pattern: &str) -> Result<Self, BuildError> {
        validate_pattern_shape(pattern)?;

        let normalized = pattern.replace('\\', "/");
        let static_root = static_root_of(&normalized);

        let matcher = globset::GlobBuilder::new(&normalized)
            .literal_separator(true)
            .build()
            .map_err(|err| BuildError::InvalidGlob {
                pattern: pattern.to_owned(),
                message: err.to_string(),
            })?
            .compile_matcher();

        Ok(Self {
            pattern: pattern.to_owned(),
            matcher,
            static_root,
        })
    }

    /// True when `relative` (already `/`-separated) matches this pattern.
    pub fn is_match(&self, relative: &str) -> bool {
        self.matcher.is_match(relative)
    }

    /// Map a matched source-relative path to the path appended to the destination.
    pub fn destination_suffix(&self, source_relative: &str) -> Result<String, BuildError> {
        match &self.static_root {
            None => Ok(source_relative.to_owned()),
            Some(root) if root.is_empty() => Ok(source_relative.to_owned()),
            Some(root) => {
                let prefix = format!("{root}/");
                source_relative
                    .strip_prefix(&prefix)
                    .map(str::to_owned)
                    .ok_or_else(|| BuildError::UnsafeRelativePath {
                        path: source_relative.to_owned(),
                        reason: format!("match is not under the pattern static root `{root}`"),
                    })
            }
        }
    }
}

fn validate_pattern_shape(pattern: &str) -> Result<(), BuildError> {
    if pattern.is_empty() {
        return Err(BuildError::InvalidGlob {
            pattern: pattern.to_owned(),
            message: "pattern must not be empty".to_owned(),
        });
    }

    let normalized = pattern.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(BuildError::InvalidGlob {
            pattern: pattern.to_owned(),
            message: "pattern must be relative to the source root".to_owned(),
        });
    }

    for part in normalized.split('/') {
        if part == ".." {
            return Err(BuildError::InvalidGlob {
                pattern: pattern.to_owned(),
                message: "pattern must not contain `..`".to_owned(),
            });
        }
        if part.is_empty() {
            return Err(BuildError::InvalidGlob {
                pattern: pattern.to_owned(),
                message: "pattern must not contain empty path components".to_owned(),
            });
        }
    }

    Ok(())
}

/// Longest leading directory prefix with no glob metacharacters.
///
/// If the pattern has no metacharacters at all, the parent directory is the
/// static root so a literal file keeps its filename in the destination.
///
/// ```text
/// **/*             -> ""            (preserve entire relative path)
/// bin/**/*         -> "bin"
/// assets/icons/*.png -> "assets/icons"
/// a/foo.dll        -> "a"
/// foo.exe          -> ""
/// ```
fn static_root_of(pattern: &str) -> Option<String> {
    let parts: Vec<&str> = pattern.split('/').collect();
    if parts.is_empty() {
        return None;
    }

    let first_pattern = parts.iter().position(|part| is_pattern_component(part));

    let static_len = match first_pattern {
        // Fully static path: strip only the final filename component.
        None => parts.len().saturating_sub(1),
        Some(index) => index,
    };

    if static_len == 0 {
        Some(String::new())
    } else {
        Some(parts[..static_len].join("/"))
    }
}

fn is_pattern_component(component: &str) -> bool {
    component
        .chars()
        .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_roots() {
        assert_eq!(static_root_of("**/*"), Some(String::new()));
        assert_eq!(static_root_of("bin/**/*"), Some("bin".to_owned()));
        assert_eq!(
            static_root_of("assets/icons/*.png"),
            Some("assets/icons".to_owned())
        );
        assert_eq!(static_root_of("*.exe"), Some(String::new()));
        assert_eq!(static_root_of("a/foo.dll"), Some("a".to_owned()));
        assert_eq!(static_root_of("foo.exe"), Some(String::new()));
        assert_eq!(
            static_root_of("foo/bar/baz.txt"),
            Some("foo/bar".to_owned())
        );
        assert_eq!(static_root_of("foo/**/bar.txt"), Some("foo".to_owned()));
    }

    #[test]
    fn destination_suffix_strips_static_root() {
        let pattern = FilePattern::compile("bin/**/*").unwrap();
        assert_eq!(
            pattern.destination_suffix("bin/acme.exe").unwrap(),
            "acme.exe"
        );
        assert_eq!(
            pattern.destination_suffix("bin/helpers/foo.dll").unwrap(),
            "helpers/foo.dll"
        );
    }

    #[test]
    fn destination_suffix_preserves_full_path_for_recursive_root() {
        let pattern = FilePattern::compile("**/*").unwrap();
        assert_eq!(
            pattern.destination_suffix("a/b/c.txt").unwrap(),
            "a/b/c.txt"
        );
    }
}
