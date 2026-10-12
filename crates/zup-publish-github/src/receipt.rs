use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;
use zup_publish::{Notice, ProductState, PublicationState, PublishReceipt, PublishedProduct};

use crate::repository::GithubRepository;

pub const RECEIPT_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubAsset {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub state: ProductState,
    pub digest: Sha256Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubReceipt {
    pub schema: u32,
    pub host: String,
    pub repository: String,
    pub tag: String,
    pub release_id: u64,
    pub state: PublicationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,
    pub assets: Vec<GithubAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub immutable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<Notice>,
}

impl GithubReceipt {
    pub fn new(repository: &GithubRepository, tag: &str, release_id: u64) -> Self {
        Self {
            schema: RECEIPT_SCHEMA,
            host: repository.host.host.clone(),
            repository: repository.to_string(),
            tag: tag.to_owned(),
            release_id,
            state: PublicationState::Planned,
            url: None,
            target_commitish: None,
            assets: Vec::new(),
            immutable: None,
            attestation: None,
            notices: Vec::new(),
        }
    }

    pub fn asset(&self, name: &str) -> Option<&GithubAsset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    pub fn to_publish_receipt(&self) -> PublishReceipt {
        let mut receipt = PublishReceipt::new("github", &self.repository, &self.tag);
        receipt.state = self.state;
        receipt.reference = self.release_id.to_string();
        receipt.url.clone_from(&self.url);
        receipt.notices = self.notices.clone();
        receipt.products = self
            .assets
            .iter()
            .map(|asset| PublishedProduct {
                name: asset.name.clone(),
                digest: asset.digest,
                size: asset.size,
                state: asset.state,
                url: asset.download_url.clone(),
            })
            .collect();
        receipt
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec_pretty(self).map_err(|error| error.to_string())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let receipt: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if receipt.schema != RECEIPT_SCHEMA {
            return Err(format!(
                "the GitHub receipt is schema {}; this build reads schema {RECEIPT_SCHEMA}",
                receipt.schema
            ));
        }
        Ok(receipt)
    }
}
