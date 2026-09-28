//! The publication, provider-neutrally.
//!
//! `PublishReport` is a good model of what a publication *did* — phases, steps, bytes
//! moved, notices about release integrity — and it stays exactly that, inside
//! `zup-publish`, where the publisher writes it. It is not a wire contract: a consumer
//! that wanted to learn a release URL should not have to walk a provider's phase list,
//! and a contract shaped like a phase list freezes the provider's step order into
//! something every other provider has to imitate.
//!
//! So this is the projection: where it went, under what name, in what state, at what
//! address, with which files. `provider` and `subject` are strings rather than enums
//! for the same reason everything else here is an open vocabulary — `github` and
//! `github.example.internal` are both legal, and a third provider is an additive
//! change.
//!
//! `immutable` is a question, not a claim. A host that will not let a published asset
//! be replaced says so; a host whose draft is editable says nothing. `null` is the
//! honest answer for the second, and a consumer that assumed `true` would be promising
//! the user something nobody established.

use serde::{Deserialize, Serialize};

use crate::artifact::{ByteCount, Digest};
use crate::identifier::Identifier;

/// Where a release went, and what state it is in.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    /// The provider, e.g. `github`.
    pub provider: String,
    /// Where it went, in the provider's own terms: `owner/name`, or
    /// `host/owner/name` on an installation that is not the public one.
    pub subject: String,
    /// The tag the release answers to.
    pub tag: String,
    /// The provider's identifier for it, when it has one.
    ///
    /// A string because whether a provider's id fits in a double is the provider's
    /// business. Encoding it as a number is how a release id silently becomes
    /// `1.2345678901234568e18` somewhere between the host and a log line.
    pub id: Option<String>,
    /// `published`, `draft`, `prerelease`, `deleted`, or whatever the provider says.
    pub state: Identifier,
    /// The address a person can open, when there is one.
    pub url: Option<String>,
    /// Whether the host refuses to replace these bytes. `null` when it did not say.
    pub immutable: Option<bool>,
    /// The files the release carries.
    pub assets: Vec<PublicationAsset>,
    /// The provider's own receipt, project-relative, when one was written.
    ///
    /// Named rather than inlined: the receipt is the provider's document in the
    /// provider's shape, and a contract that absorbed it would have to version it
    /// separately. A machine consumer that wants the tag, the state and the asset
    /// names does not need to read it.
    pub receipt: Option<String>,
}

/// One file a published release carries.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationAsset {
    /// The name the release carries it under. Flat, because that is how a host
    /// addresses an asset.
    pub name: String,
    /// Its size on the host.
    pub size: ByteCount,
    /// The digest zup published, when it published one.
    pub digest: Option<Digest>,
    /// `uploaded`, `existing`, `replaced`, or whatever the provider reported.
    pub state: Identifier,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A release identifier that does not fit in a double must reach the wire as a
    /// string. This is the failure the string is here to prevent, and it is a wire-shape
    /// claim rather than a round trip: a number would lose the last digit on the way
    /// through any consumer that parses numbers as doubles.
    #[test]
    fn a_provider_identifier_that_would_lose_precision_is_a_string() {
        let publication = Publication {
            provider: "github".to_owned(),
            subject: "acme/acme".to_owned(),
            tag: "v1.4.0".to_owned(),
            id: Some("1234567890123456789".to_owned()),
            state: Identifier::fixed("published"),
            url: Some("https://github.com/acme/acme/releases/tag/v1.4.0".to_owned()),
            immutable: Some(true),
            assets: Vec::new(),
            receipt: Some("dist/github-publish.json".to_owned()),
        };
        let value = serde_json::to_value(&publication).unwrap();
        assert!(value["id"].is_string(), "{}", value["id"]);
        assert_eq!(value["id"], "1234567890123456789");
    }
}
