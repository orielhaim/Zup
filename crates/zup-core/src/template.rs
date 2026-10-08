use std::borrow::Cow;
use std::fmt;

#[cfg(feature = "schema")]
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TemplateError {
    #[error("unterminated template variable")]
    UnterminatedVariable,

    #[error("empty template variable")]
    EmptyVariable,

    #[error("unknown template variable `{name}`")]
    UnknownVariable { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Variable {
    AppId,
    AppName,
    AppVersion,
    Install,
    Location(crate::InstallLocation),
}

impl Variable {
    pub fn parse(name: &str) -> Result<Self, TemplateError> {
        if let Some(location) = name.strip_prefix("location.")
            && let Some(location) = crate::InstallLocation::parse(location)
        {
            return Ok(Self::Location(location));
        }
        match name {
            "app.id" => Ok(Self::AppId),
            "app.name" => Ok(Self::AppName),
            "app.version" => Ok(Self::AppVersion),
            "install" => Ok(Self::Install),
            other => Err(TemplateError::UnknownVariable {
                name: other.to_owned(),
            }),
        }
    }

    pub fn as_str(self) -> Cow<'static, str> {
        match self {
            Self::AppId => Cow::Borrowed("app.id"),
            Self::AppName => Cow::Borrowed("app.name"),
            Self::AppVersion => Cow::Borrowed("app.version"),
            Self::Install => Cow::Borrowed("install"),
            Self::Location(location) => Cow::Owned(format!("location.{location}")),
        }
    }
}

impl fmt::Display for Variable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplatePart {
    Literal(String),
    Variable(Variable),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableValue {
    Literal(String),
    Template(Template),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Template {
    parts: Vec<TemplatePart>,
}

impl Template {
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

    pub fn contains_variable(&self, variable: Variable) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, TemplatePart::Variable(v) if *v == variable))
    }

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

        parts.retain(|part| !matches!(part, TemplatePart::Literal(lit) if lit.is_empty()));
        Template { parts }
    }

    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
            || self
                .parts
                .iter()
                .all(|part| matches!(part, TemplatePart::Literal(lit) if lit.is_empty()))
    }

    pub fn as_literal(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [] => Some(""),
            [TemplatePart::Literal(lit)] => Some(lit),
            _ => None,
        }
    }

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

#[cfg(feature = "schema")]
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
            "description": "A path or command template using zup variables such as ${install} and ${location.user_data}.",
            "examples": ["${install}/app.exe", "${location.programs}/Acme"]
        })
    }
}
