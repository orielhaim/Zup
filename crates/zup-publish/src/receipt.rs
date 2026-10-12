use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;

pub const RECEIPT_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationState {
    Published,
    Unchanged,
    Draft,
    Planned,
}

impl PublicationState {
    pub const ALL: [PublicationState; 4] = [
        PublicationState::Published,
        PublicationState::Unchanged,
        PublicationState::Draft,
        PublicationState::Planned,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Unchanged => "unchanged",
            Self::Draft => "draft",
            Self::Planned => "planned",
        }
    }

    pub const fn is_public(self) -> bool {
        matches!(self, Self::Published | Self::Unchanged)
    }
}

pub const PUBLICATION_STATES: [&str; 4] = ["published", "unchanged", "draft", "planned"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductState {
    Uploaded,
    AlreadyPresent,
    Replaced,
    Planned,
}

impl ProductState {
    pub const ALL: [ProductState; 4] = [
        ProductState::Uploaded,
        ProductState::AlreadyPresent,
        ProductState::Replaced,
        ProductState::Planned,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uploaded => "uploaded",
            Self::AlreadyPresent => "already_present",
            Self::Replaced => "replaced",
            Self::Planned => "planned",
        }
    }
}

pub const PRODUCT_STATES: [&str; 4] = ["uploaded", "already_present", "replaced", "planned"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedProduct {
    pub name: String,
    pub digest: Sha256Digest,
    pub size: u64,
    pub state: ProductState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub label: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Notice {
    pub fn ok(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: "ok".to_owned(),
            detail: None,
        }
    }

    pub fn warn(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: "warn".to_owned(),
            detail: Some(detail.into()),
        }
    }

    pub fn info(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: "info".to_owned(),
            detail: Some(detail.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishReceipt {
    pub schema: u32,
    pub provider: String,
    pub subject: String,
    pub tag: String,
    pub state: PublicationState,
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub products: Vec<PublishedProduct>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<Notice>,
}

impl PublishReceipt {
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

    pub fn bytes(&self) -> u64 {
        self.products
            .iter()
            .fold(0u64, |sum, product| sum.saturating_add(product.size))
    }

    pub fn product(&self, name: &str) -> Option<&PublishedProduct> {
        self.products.iter().find(|product| product.name == name)
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec_pretty(self).map_err(|error| error.to_string())
    }

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
