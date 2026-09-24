//! Structural templates with a fixed variable vocabulary.
//!
//! Templates are parsed into parts. Variables are not resolved here.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Errors produced while parsing a template string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TemplateError {
    /// A `${...` sequence was not closed with `}`.
    #[error("unterminated template variable")]
    UnterminatedVariable,

    /// A `${}` sequence had an empty name.
    #[error("empty template variable")]
    EmptyVariable,

    /// The variable name is not in the supported vocabulary.
    #[error("unknown template variable `{name}`")]
    UnknownVariable { name: String },
}

/// Supported template variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Variable {
    AppId,
    AppName,
    AppVersion,
    Install,
    KnownProgramFiles,
    KnownLocalAppData,
    KnownProgramData,
    KnownStartMenu,
    KnownDesktop,
}

impl Variable {
    /// Parse a variable name such as `install` or `known.program_files`.
    pub fn parse(name: &str) -> Result<Self, TemplateError> {
        match name {
            "app.id" => Ok(Self::AppId),
            "app.name" => Ok(Self::AppName),
            "app.version" => Ok(Self::AppVersion),
            "install" => Ok(Self::Install),
            "known.program_files" => Ok(Self::KnownProgramFiles),
            "known.local_app_data" => Ok(Self::KnownLocalAppData),
            "known.program_data" => Ok(Self::KnownProgramData),
            "known.start_menu" => Ok(Self::KnownStartMenu),
            "known.desktop" => Ok(Self::KnownDesktop),
            other => Err(TemplateError::UnknownVariable {
                name: other.to_owned(),
            }),
        }
    }

    /// The canonical variable name used inside `${...}`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AppId => "app.id",
            Self::AppName => "app.name",
            Self::AppVersion => "app.version",
            Self::Install => "install",
            Self::KnownProgramFiles => "known.program_files",
            Self::KnownLocalAppData => "known.local_app_data",
            Self::KnownProgramData => "known.program_data",
            Self::KnownStartMenu => "known.start_menu",
            Self::KnownDesktop => "known.desktop",
        }
    }
}

impl fmt::Display for Variable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One piece of a parsed template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplatePart {
    /// Verbatim text.
    Literal(String),
    /// A variable placeholder.
    Variable(Variable),
}

/// How a variable resolves during [`Template::substitute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableValue {
    /// Replace with verbatim text.
    Literal(String),
    /// Replace with another template's parts.
    Template(Template),
}

/// A string containing zero or more `${...}` variables.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Template {
    parts: Vec<TemplatePart>,
}

impl Template {
    /// Parse a template string into literal and variable parts.
    pub fn parse(source: &str) -> Result<Self, TemplateError> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = source.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '$' && chars.peek() == Some(&'{') {
                chars.next();
                if !literal.is_empty() {
                    parts.push(TemplatePart::Literal(std::mem::take(&mut literal)));
                }

                let mut name = String::new();
                let mut terminated = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        terminated = true;
                        break;
                    }
                    name.push(c);
                }
                if !terminated {
                    return Err(TemplateError::UnterminatedVariable);
                }

                if name.is_empty() {
                    return Err(TemplateError::EmptyVariable);
                }
                if name.contains('$') {
                    return Err(TemplateError::UnterminatedVariable);
                }

                parts.push(TemplatePart::Variable(Variable::parse(&name)?));
            } else {
                literal.push(c);
            }
        }

        if !literal.is_empty() {
            parts.push(TemplatePart::Literal(literal));
        }

        Ok(Self { parts })
    }

    /// True when the template references `variable`.
    pub fn contains_variable(&self, variable: Variable) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, TemplatePart::Variable(v) if *v == variable))
    }

    /// Substitute known variables structurally.
    ///
    /// Returning `None` leaves the variable unresolved. Adjacent literals are
    /// merged so the result stays normalized.
    pub fn substitute<F>(&self, mut resolve: F) -> Template
    where
        F: FnMut(Variable) -> Option<VariableValue>,
    {
        let mut parts: Vec<TemplatePart> = Vec::new();

        for part in &self.parts {
            match part {
                TemplatePart::Literal(lit) => push_literal(&mut parts, lit),
                TemplatePart::Variable(var) => match resolve(*var) {
                    None => parts.push(TemplatePart::Variable(*var)),
                    Some(VariableValue::Literal(text)) => push_literal(&mut parts, &text),
                    Some(VariableValue::Template(nested)) => {
                        for nested_part in nested.parts {
                            match nested_part {
                                TemplatePart::Literal(lit) => push_literal(&mut parts, &lit),
                                TemplatePart::Variable(var) => {
                                    parts.push(TemplatePart::Variable(var));
                                }
                            }
                        }
                    }
                },
            }
        }

        // Drop empty literals produced by substitution.
        parts.retain(|part| !matches!(part, TemplatePart::Literal(lit) if lit.is_empty()));
        Template { parts }
    }

    /// Parsed template parts in source order.
    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }

    /// True when the template contains no characters or variables.
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
            || self
                .parts
                .iter()
                .all(|part| matches!(part, TemplatePart::Literal(lit) if lit.is_empty()))
    }

    /// The literal text when the template has no variables.
    pub fn as_literal(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [] => Some(""),
            [TemplatePart::Literal(lit)] => Some(lit),
            _ => None,
        }
    }

    /// Append a portable relative path as a literal `/`-separated suffix.
    pub fn join_relative(&self, relative: &crate::path::RelativePath) -> Self {
        let mut parts = self.parts.clone();
        let suffix = relative.as_str();
        if suffix.is_empty() {
            return Self { parts };
        }

        match parts.last_mut() {
            Some(TemplatePart::Literal(lit)) => {
                if lit.is_empty() {
                    *lit = suffix.to_owned();
                } else if lit.ends_with('/') {
                    lit.push_str(suffix);
                } else {
                    lit.push('/');
                    lit.push_str(suffix);
                }
            }
            Some(_) => {
                parts.push(TemplatePart::Literal(format!("/{suffix}")));
            }
            None => parts.push(TemplatePart::Literal(suffix.to_owned())),
        }

        Self { parts }
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for part in &self.parts {
            match part {
                TemplatePart::Literal(lit) => f.write_str(lit)?,
                TemplatePart::Variable(var) => write!(f, "${{{var}}}")?,
            }
        }
        Ok(())
    }
}

fn push_literal(parts: &mut Vec<TemplatePart>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(TemplatePart::Literal(existing)) = parts.last_mut() {
        existing.push_str(text);
    } else {
        parts.push(TemplatePart::Literal(text.to_owned()));
    }
}

impl<'de> Deserialize<'de> for Template {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl Serialize for Template {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl JsonSchema for Template {
    fn schema_name() -> Cow<'static, str> {
        "Template".into()
    }

    fn schema_id() -> Cow<'static, str> {
        "zup_core::Template".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "A path or command template using zup variables such as ${install} and ${known.local_app_data}.",
            "examples": ["${install}/app.exe", "${known.program_files}/Acme"]
        })
    }
}
