//! The receipt a GitHub publication leaves behind.
//!
//! # Why this is not `zup-release.json`
//!
//! Because `zup-release.json` is portable and a release id is not. The same
//! application, the same version, and the same files published to two forges
//! produce two different sets of identifiers, and a release manifest that
//! carried one would have to be rewritten for the other. Worse, it would stop
//! being a statement about *what was released* and become a statement about
//! *where it went*, which is a different fact with a different lifetime.
//!
//! So the two documents are separate, this one is provider-specific, and the
//! neutral shape in `zup-publish` is a projection of it rather than its
//! container.
//!
//! # What is in it
//!
//! The release id, every asset id, the state each asset reached, the remote
//! digests GitHub computed, and the integrity facts GitHub reported. That is
//! what resume, a CI summary, and a bug report three weeks later all need.
//!
//! # What is not in it
//!
//! A token. There is no field for one, and that is the whole design: a receipt
//! is uploaded, attached, pasted, and archived, and a type that cannot hold a
//! secret cannot leak one.

use serde::{Deserialize, Serialize};
use zup_core::Sha256Digest;
use zup_publish::{Notice, ProductState, PublicationState, PublishReceipt, PublishedProduct};

use crate::repository::GithubRepository;

/// Version of the GitHub receipt shape.
pub const RECEIPT_SCHEMA: u32 = 1;

/// One asset, as GitHub now holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubAsset {
    /// GitHub's own identifier for the asset.
    pub id: u64,
    /// The name in the release.
    pub name: String,
    pub size: u64,
    /// How the publication reached it.
    pub state: ProductState,
    /// The digest GitHub computed over the bytes it received.
    ///
    /// This is the value that had to match the local digest before the asset was
    /// called published, so it is the receipt's central fact: it is proof that
    /// what is on the host is what was signed and attested locally.
    pub digest: Sha256Digest,
    /// The address a browser would use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
}

/// What one GitHub publication did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubReceipt {
    pub schema: u32,
    /// The installation's web hostname, so a receipt from a fork is distinguishable.
    pub host: String,
    pub repository: String,
    pub tag: String,
    /// GitHub's release id.
    pub release_id: u64,
    pub state: PublicationState,
    /// The address of the release page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The commit the release was made from, when GitHub reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,
    pub assets: Vec<GithubAsset>,
    /// Whether the release is immutable, when GitHub says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub immutable: Option<bool>,
    /// Whether GitHub holds a release attestation for it, when GitHub says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<Notice>,
}

impl GithubReceipt {
    /// An empty receipt for one release.
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

    /// The asset with `name`.
    pub fn asset(&self, name: &str) -> Option<&GithubAsset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    /// The neutral projection, for a consumer that reads receipts from more than
    /// one provider.
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

    /// The receipt as JSON.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec_pretty(self).map_err(|error| error.to_string())
    }

    /// Read a receipt.
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
