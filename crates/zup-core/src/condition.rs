use std::collections::BTreeSet;
use std::fmt;

#[cfg(feature = "schema")]
use std::borrow::Cow;

#[cfg(feature = "schema")]
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::ids::ComponentId;
use crate::ids::ValueError;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConditionError {
    #[error("invalid condition: unexpected end of expression")]
    UnexpectedEnd,

    #[error("invalid condition: unexpected token `{token}`")]
    UnexpectedToken { token: String },

    #[error("invalid condition: expected component id string")]
    ExpectedComponentId,

    #[error("invalid condition: {0}")]
    InvalidComponentId(#[from] ValueError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    Component(ComponentId),
    Not(Box<Condition>),
    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
}

impl Condition {
    pub fn parse(source: &str) -> Result<Self, ConditionError> {
        let tokens = tokenize(source)?;
        let mut parser = Parser { tokens, index: 0 };
        let condition = parser.parse_or()?;
        if parser.peek().is_some() {
            let token = format!("{:?}", parser.tokens[parser.index]);
            return Err(ConditionError::UnexpectedToken { token });
        }
        Ok(condition)
    }

    pub fn evaluate(&self, selected: &BTreeSet<ComponentId>) -> bool {
        match self {
            Self::Component(id) => selected.contains(id),
            Self::Not(inner) => !inner.evaluate(selected),
            Self::And(lhs, rhs) => lhs.evaluate(selected) && rhs.evaluate(selected),
            Self::Or(lhs, rhs) => lhs.evaluate(selected) || rhs.evaluate(selected),
        }
    }

    pub fn referenced_components(&self) -> BTreeSet<ComponentId> {
        let mut out = BTreeSet::new();
        self.collect_components(&mut out);
        out
    }

    fn collect_components(&self, out: &mut BTreeSet<ComponentId>) {
        match self {
            Self::Component(id) => {
                out.insert(id.clone());
            }
            Self::Not(inner) => inner.collect_components(out),
            Self::And(lhs, rhs) | Self::Or(lhs, rhs) => {
                lhs.collect_components(out);
                rhs.collect_components(out);
            }
        }
    }
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_prec(f, 0)
    }
}

impl Condition {
    fn fmt_prec(&self, f: &mut fmt::Formatter<'_>, prec: u8) -> fmt::Result {
        match self {
            Self::Component(id) => write!(f, "component(\"{id}\")"),
            Self::Not(inner) => {
                if prec > 1 {
                    f.write_str("(")?;
                }
                f.write_str("!")?;
                inner.fmt_prec(f, 1)?;
                if prec > 1 {
                    f.write_str(")")?;
                }
                Ok(())
            }
            Self::And(lhs, rhs) => {
                if prec > 2 {
                    f.write_str("(")?;
                }
                lhs.fmt_prec(f, 2)?;
                f.write_str(" && ")?;
                rhs.fmt_prec(f, 2)?;
                if prec > 2 {
                    f.write_str(")")?;
                }
                Ok(())
            }
            Self::Or(lhs, rhs) => {
                if prec > 3 {
                    f.write_str("(")?;
                }
                lhs.fmt_prec(f, 3)?;
                f.write_str(" || ")?;
                rhs.fmt_prec(f, 3)?;
                if prec > 3 {
                    f.write_str(")")?;
                }
                Ok(())
            }
        }
    }
}

impl<'de> Deserialize<'de> for Condition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl Serialize for Condition {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

#[cfg(feature = "schema")]
impl JsonSchema for Condition {
    fn schema_name() -> Cow<'static, str> {
        "Condition".into()
    }

    fn schema_id() -> Cow<'static, str> {
        "zup_core::Condition".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "A component expression such as component(\"docs\") or !component(\"debug\").",
            "examples": ["component(\"docs\")", "!component(\"debug\")"]
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Component,
    Not,
    And,
    Or,
    Open,
    Close,
    String(String),
}

fn tokenize(source: &str) -> Result<Vec<Token>, ConditionError> {
    let mut tokens = Vec::new();
    let mut chars = source.chars().peekable();

    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }

        match c {
            '!' => {
                chars.next();
                tokens.push(Token::Not);
            }
            '&' => {
                chars.next();
                if chars.peek() != Some(&'&') {
                    return Err(ConditionError::UnexpectedToken {
                        token: "&".to_owned(),
                    });
                }
                chars.next();
                tokens.push(Token::And);
            }
            '|' => {
                chars.next();
                if chars.peek() != Some(&'|') {
                    return Err(ConditionError::UnexpectedToken {
                        token: "|".to_owned(),
                    });
                }
                chars.next();
                tokens.push(Token::Or);
            }
            '(' => {
                chars.next();
                tokens.push(Token::Open);
            }
            ')' => {
                chars.next();
                tokens.push(Token::Close);
            }
            '"' => {
                chars.next();
                let mut value = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '"' {
                        closed = true;
                        break;
                    }
                    value.push(c);
                }
                if !closed {
                    return Err(ConditionError::UnexpectedEnd);
                }
                tokens.push(Token::String(value));
            }
            _ if c.is_ascii_alphabetic() => {
                let mut ident = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphabetic() {
                        ident.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                match ident.as_str() {
                    "component" => tokens.push(Token::Component),
                    other => {
                        return Err(ConditionError::UnexpectedToken {
                            token: other.to_owned(),
                        });
                    }
                }
            }
            other => {
                return Err(ConditionError::UnexpectedToken {
                    token: other.to_string(),
                });
            }
        }
    }

    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn bump(&mut self) -> Option<&Token> {
        let token = self.tokens.get(self.index);
        if token.is_some() {
            self.index += 1;
        }
        token
    }

    fn parse_or(&mut self) -> Result<Condition, ConditionError> {
        let mut lhs = self.parse_and()?;
        while self.peek() == Some(&Token::Or) {
            self.bump();
            let rhs = self.parse_and()?;
            lhs = Condition::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Condition, ConditionError> {
        let mut lhs = self.parse_unary()?;
        while self.peek() == Some(&Token::And) {
            self.bump();
            let rhs = self.parse_unary()?;
            lhs = Condition::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Condition, ConditionError> {
        if self.peek() == Some(&Token::Not) {
            self.bump();
            let inner = self.parse_unary()?;
            return Ok(Condition::Not(Box::new(inner)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Condition, ConditionError> {
        match self.bump() {
            Some(Token::Open) => {
                let inner = self.parse_or()?;
                if self.bump() != Some(&Token::Close) {
                    return Err(ConditionError::UnexpectedEnd);
                }
                Ok(inner)
            }
            Some(Token::Component) => {
                if self.bump() != Some(&Token::Open) {
                    return Err(ConditionError::UnexpectedToken {
                        token: "(".to_owned(),
                    });
                }
                let Some(Token::String(id)) = self.bump().cloned() else {
                    return Err(ConditionError::ExpectedComponentId);
                };
                if self.bump() != Some(&Token::Close) {
                    return Err(ConditionError::UnexpectedToken {
                        token: ")".to_owned(),
                    });
                }
                Ok(Condition::Component(ComponentId::new(id)?))
            }
            Some(other) => Err(ConditionError::UnexpectedToken {
                token: format!("{other:?}"),
            }),
            None => Err(ConditionError::UnexpectedEnd),
        }
    }
}
