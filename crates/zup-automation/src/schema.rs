//! The language-neutral contract, derived from the same types the Action's
//! TypeScript comes from.
//!
//! One source, two artifacts, one command: `cargo xtask automation generate`. The JSON
//! Schema is what a consumer in a language that is not TypeScript reads, and it is
//! derived rather than authored for the same reason the manifest schema is — a
//! document nobody derives from the types drifts, and the drift is invisible until a
//! consumer is built against a shape zup stopped producing.
//!
//! Two properties are set by hand, and both matter:
//!
//! - **Nothing is closed.** No definition carries `additionalProperties: false`. A
//!   consumer is required to ignore fields it does not know, so a schema that rejected
//!   them would describe a protocol that does not exist.
//! - **The envelope's fields are required.** `AutomationResult` always writes all ten,
//!   with `null` where there is nothing, so a document that omits one is not a zup
//!   document and a validator should say so.
//!
//! This module is behind the `bindings` feature. It is reached by `zup-xtask` and by
//! nothing that ships, so `schemars` is absent from every release binary.

use schemars::{JsonSchema, Schema, generate::SchemaSettings, json_schema};
use serde_json::{Map, Value, json};

use crate::artifact::MAX_SAFE_BYTES;

/// The `$id` of the committed schema.
pub const SCHEMA_ID: &str = "https://zup.dev/schema/automation-v1.schema.json";

/// The contract document, as pretty JSON with a trailing newline.
pub fn schema_json() -> String {
    let mut text = serde_json::to_string_pretty(&document()).expect("a schema this crate built");
    text.push('\n');
    text
}

/// The two roots a consumer can be handed, generated from the types themselves.
///
/// A fresh generator per root rather than one generator for both: `into_root_schema_for`
/// consumes it, and sharing one would mean the second root's definitions were
/// unreachable from the first. The merge below is what makes them one document, and
/// each root is filed under its own name because `into_root_schema_for` inlines the
/// root's body and only puts its dependencies in `$defs`.
pub fn roots() -> [(&'static str, Schema); 2] {
    [
        (
            "AutomationResult",
            root::<crate::result::AutomationResult>(),
        ),
        ("StreamEvent", root::<crate::stream::StreamEvent>()),
    ]
}

fn root<T: JsonSchema>() -> Schema {
    SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<T>()
}

/// The two root documents a consumer can be handed.
///
/// `--format json` writes the first. `--format jsonl` writes the second, repeatedly,
/// with the first as its first line and a result inside its last. One `oneOf` rather
/// than two files, because a consumer reading either one benefits from the definitions
/// the other needs.
fn document() -> Value {
    let mut definitions = Map::new();
    for (name, root) in roots() {
        merge_definitions(&mut definitions, name, &root);
    }
    let mut document = Map::new();
    document.insert("$id".into(), json!(SCHEMA_ID));
    document.insert(
        "$schema".into(),
        json!("https://json-schema.org/draft/2020-12/schema"),
    );
    document.insert("title".into(), json!("zup automation protocol"));
    document.insert(
        "description".into(),
        json!(
            "What zup's developer CLI writes to stdout under `--format json` and \
             `--format jsonl`. A consumer accepts a document whose `protocol` shares its \
             major version, and ignores fields, event types, operation names, artifact \
             kinds and diagnostic codes it does not know. A different major is refused. \
             Nothing here is closed: no definition sets `additionalProperties: false`."
        ),
    );
    document.insert(
        "oneOf".into(),
        json!([
            {
                "title": "final result — the whole of a `--format json` stdout",
                "$ref": "#/$defs/AutomationResult"
            },
            {
                "title": "stream message — one line of a `--format jsonl` stream",
                "$ref": "#/$defs/StreamEvent"
            },
        ]),
    );
    document.insert("$defs".into(), Value::Object(definitions));
    Value::Object(document)
}

/// Fold one generated root into the shared definition table.
///
/// `$defs` is a flat namespace in every schema, and a type reachable from both roots
/// must be defined once. The envelope *is* reachable from the stream — a `completed`
/// line carries one — so the second root's `$defs` already contains it, and its body
/// differs from the inlined root only by the `$schema` and `title` the generator adds
/// to a root. Those two keys are therefore dropped before the two are compared; a
/// difference in anything else is a schema that validates one shape two ways, which is
/// a bug here rather than a merge to resolve.
fn merge_definitions(into: &mut Map<String, Value>, name: &str, root: &Schema) {
    let mut root = serde_json::to_value(root).expect("a schema this crate built");
    let Some(object) = root.as_object_mut() else {
        return;
    };
    object.remove("$schema");
    object.remove("title");
    let definitions = object
        .remove("$defs")
        .and_then(|defs| defs.as_object().cloned())
        .unwrap_or_default();
    insert(into, name, Value::Object(std::mem::take(object)));
    for (defined, mut body) in definitions {
        if let Some(fields) = body.as_object_mut() {
            fields.remove("$schema");
            fields.remove("title");
        }
        insert(into, &defined, body);
    }
}

/// Put one definition in the table, refusing a name that already means something else.
fn insert(into: &mut Map<String, Value>, name: &str, body: Value) {
    if let Some(existing) = into.get(name) {
        assert_eq!(existing, &body, "`{name}` is defined two different ways");
        return;
    }
    into.insert(name.to_owned(), body);
}

/// A bounded integer, as the schema describes it.
fn byte_count() -> Schema {
    json_schema!({
        "type": "integer",
        "minimum": 0,
        "maximum": MAX_SAFE_BYTES,
        "description":
            "A byte count, bounded so that a consumer holding it in a 64-bit float \
             represents it exactly. A larger value is refused rather than rounded.",
    })
}

/// A lowercase dotted identifier, with the grammar as a pattern.
fn identifier(description: &str) -> Schema {
    json_schema!({
        "type": "string",
        "pattern": r"^[a-z][a-z0-9]*(-[a-z0-9]+)*(\.[a-z][a-z0-9]*(-[a-z0-9]+)*)*$",
        "description": description,
    })
}

/// `MAJOR.MINOR`, with the compatibility rule in the description rather than in a
/// custom format: a format string is opaque to every validator that is not the one that
/// wrote it.
fn protocol_version() -> Schema {
    json_schema!({
        "type": "string",
        "pattern": r"^[0-9]+\.[0-9]+$",
        "description":
            "MAJOR.MINOR. A consumer accepts any version with the same major and refuses \
             any other.",
    })
}

impl JsonSchema for crate::artifact::ByteCount {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ByteCount".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> Schema {
        byte_count()
    }
}

impl JsonSchema for crate::identifier::Identifier {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Identifier".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> Schema {
        identifier(
            "A lowercase dotted identifier. The vocabulary is open: an unknown operation \
             name, artifact kind, mode, publication state or diagnostic code is an \
             additive change, and a consumer displays it rather than refusing it.",
        )
    }
}

impl JsonSchema for crate::version::ProtocolVersion {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ProtocolVersion".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> Schema {
        protocol_version()
    }
}
