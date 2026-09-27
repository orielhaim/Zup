//! What a publisher reports after it published.
//!
//! A receipt exists so three things are possible that otherwise are not: a
//! publication can be described to a CI log, resumed by a later command, and
//! diagnosed after the fact without a terminal session.
//!
//! It is deliberately *not* the release manifest. The release manifest is
//! portable, provider-neutral, and describes what a release is; a receipt
//! describes one publication of it, to one provider, with that provider's
//! identifiers. A release id in `zup-release.json` would make the document
//! unrepublishable anywhere else, which is the wrong trade for a file that
//! exists to be portable.
//!
//! So the two are separate types, this one is the provider-neutral shape, and a
//! provider that has identifiers of its own keeps them in its own receipt and
//! projects into this. Nothing here can hold a credential: there is no field
//! for one, and adding one would be the defect rather than the feature.

use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;

/// Version of the receipt shape.
pub const RECEIPT_SCHEMA: u32 = 1;

/// Where a publication ended.
///
/// The distinction between `Published` and `Unchanged` is the one that matters
/// for a rerun: both mean the release exists and carries the right files, and
/// only one of them means this run wrote to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationState {
    /// The release was created or updated by this run.
    Published,
    /// The release already existed and already matched the plan.
    Unchanged,
    /// The release exists as a draft and was left as one.
    Draft,
    /// Nothing was written; this was a plan.
    Planned,
}

impl PublicationState {
    /// Every state, in report order.
    pub const ALL: [PublicationState; 4] = [
        PublicationState::Published,
        PublicationState::Unchanged,
        PublicationState::Draft,
        PublicationState::Planned,
    ];

    /// The name a report uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Unchanged => "unchanged",
            Self::Draft => "draft",
            Self::Planned => "planned",
        }
    }

    /// Whether the release is publicly visible.
    pub const fn is_public(self) -> bool {
        matches!(self, Self::Published | Self::Unchanged)
    }
}

/// Every value [`PublicationState`] can take, for a report that enumerates.
pub const PUBLICATION_STATES: [&str; 4] = ["published", "unchanged", "draft", "planned"];

/// How one product reached the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductState {
    /// This run uploaded the file.
    Uploaded,
    /// The host already held exactly these bytes.
    AlreadyPresent,
    /// This run removed a failed remnant and uploaded the file again.
    Replaced,
    /// Nothing was written, because this was a plan.
    Planned,
}

impl ProductState {
    /// Every state, in report order.
    pub const ALL: [ProductState; 4] = [
        ProductState::Uploaded,
        ProductState::AlreadyPresent,
        ProductState::Replaced,
        ProductState::Planned,
    ];

    /// The name a report uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uploaded => "uploaded",
            Self::AlreadyPresent => "already_present",
            Self::Replaced => "replaced",
            Self::Planned => "planned",
        }
    }
}

/// Every value [`ProductState`] can take, for a report that enumerates.
pub const PRODUCT_STATES: [&str; 4] = ["uploaded", "already_present", "replaced", "planned"];

/// One file, as the host now holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedProduct {
    /// The name in the release.
    pub name: String,
    /// The digest both ends computed. Not an assertion: a product appears in a
    /// receipt only after this matched.
    pub digest: Sha256Digest,
    pub size: u64,
    /// How it got there.
    pub state: ProductState,
    /// Where a person can get it, when the host offers a stable address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A fact about a publication that is not a file.
///
/// Integrity claims belong here rather than in a status string, because "this
/// release is immutable" and "this release is not" are both things a publisher
/// can know, and a publisher that only ever says `published` is withholding the
/// only information that matters after the fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub label: String,
    /// `ok` for a property that holds, `warn` for one that does not, `info` for
    /// something a reader should know that is neither.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Notice {
    /// A property that holds.
    pub fn ok(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: "ok".to_owned(),
            detail: None,
        }
    }

    /// A property that does not hold, with what to do about it.
    pub fn warn(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: "warn".to_owned(),
            detail: Some(detail.into()),
        }
    }

    /// Something a reader should know.
    pub fn info(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: "info".to_owned(),
            detail: Some(detail.into()),
        }
    }
}

/// What a publisher published, in a shape that survives the process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishReceipt {
    pub schema: u32,
    /// The provider, for a consumer that reads receipts from more than one.
    ///
    /// A name, not a capability list. A receipt is a record of one publication,
    /// and a receiver that needs to act on it can ask the provider what it
    /// accepts.
    pub provider: String,
    /// Where it went, credential-free.
    pub subject: String,
    /// The tag.
    pub tag: String,
    pub state: PublicationState,
    /// The provider's own reference, when it has one.
    pub reference: String,
    /// A browser address for the release, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Every product, in name order.
    pub products: Vec<PublishedProduct>,
    /// Facts about the publication that are not files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<Notice>,
}

impl PublishReceipt {
    /// An empty receipt.
    pub fn new(
        provider: impl Into<String>,
        subject: impl Into<String>,
        tag: impl Into<String>,
    ) -> Self {
        Self {
            schema: RECEIPT_SCHEMA,
            provider: provider.into(),
            subject: subject.into(),
            tag: tag.into(),
            state: PublicationState::Planned,
            reference: String::new(),
            url: None,
            products: Vec::new(),
            notices: Vec::new(),
        }
    }

    /// The total bytes the publication carries.
    pub fn bytes(&self) -> u64 {
        self.products
            .iter()
            .fold(0u64, |sum, product| sum.saturating_add(product.size))
    }

    /// The product with `name`, if the receipt has it.
    pub fn product(&self, name: &str) -> Option<&PublishedProduct> {
        self.products.iter().find(|product| product.name == name)
    }

    /// The receipt as JSON.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec_pretty(self).map_err(|error| error.to_string())
    }

    /// Read a receipt.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let receipt: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if receipt.schema != RECEIPT_SCHEMA {
            return Err(format!(
                "the receipt is schema {}; this build reads schema {RECEIPT_SCHEMA}",
                receipt.schema
            ));
        }
        Ok(receipt)
    }
}
