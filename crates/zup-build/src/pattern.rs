use globset::GlobMatcher;

use crate::error::BuildError;

#[derive(Debug, Clone)]
pub struct FilePattern {
    pub pattern: String,
    pub matcher: GlobMatcher,
    pub static_root: Option<String>,
}

impl FilePattern {
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

    pub fn is_match(&self, relative: &str) -> bool {
        self.matcher.is_match(relative)
    }

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

fn static_root_of(pattern: &str) -> Option<String> {
    let parts: Vec<&str> = pattern.split('/').collect();
    if parts.is_empty() {
        return None;
    }

    let first_pattern = parts.iter().position(|part| is_pattern_component(part));

    let static_len = match first_pattern {
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
    fn destination_suffix_strips_the_static_root() {
        for (pattern, matched, suffix) in [
            ("bin/**/*", "bin/acme.exe", "acme.exe"),
            ("bin/**/*", "bin/helpers/foo.dll", "helpers/foo.dll"),
            ("**/*", "a/b/c.txt", "a/b/c.txt"),
            ("assets/icons/*.png", "assets/icons/app.png", "app.png"),
            ("*.exe", "acme.exe", "acme.exe"),
        ] {
            let compiled = FilePattern::compile(pattern).unwrap();
            assert_eq!(compiled.destination_suffix(matched).unwrap(), suffix);
        }
    }
}
