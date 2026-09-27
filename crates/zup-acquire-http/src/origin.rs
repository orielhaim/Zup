//! Where content comes from, in the order it is preferred.
//!
//! An origin is a base URL. Nothing about it is trusted: it says where bytes
//! might be, and every byte it sends is verified against an authenticated digest
//! before it is published. A mirror never becomes believed because it came
//! first, or because it is on the same network, or because it is faster.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use url::Url;
use zup_acquire::{RelativeContentPath, check_channel};

/// One place content might be served from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    url: Url,
}

impl Origin {
    /// Parse an origin, refusing anything that is not a plain base URL.
    ///
    /// A bootstrapper's origin comes from authenticated metadata or from an
    /// embedded configuration, so it still has to be a shape a request can be
    /// made against safely: HTTPS for anything remote, no embedded credentials,
    /// no query, no fragment.
    pub fn parse(value: &str) -> Result<Self, String> {
        let url = Url::parse(value).map_err(|error| error.to_string())?;
        Self::from_url(url)
    }

    /// An origin for a local filesystem repository, for tests and for an
    /// offline mirror on disk.
    pub fn file(path: &std::path::Path) -> Result<Self, String> {
        let url =
            Url::from_file_path(path).map_err(|()| "the path is not a file URL".to_owned())?;
        Self::from_url(url)
    }

    fn from_url(mut url: Url) -> Result<Self, String> {
        let local = url.scheme() == "file";
        if !local && url.scheme() != "https" && !(url.scheme() == "http" && is_loopback(&url)) {
            return Err(format!(
                "an origin must be https, a local file, or a loopback address, not `{}`",
                url.scheme()
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("an origin cannot carry credentials in its URL".to_owned());
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err("an origin cannot carry a query or a fragment".to_owned());
        }
        if !local && url.host_str().is_none_or(str::is_empty) {
            return Err("an origin must name a host".to_owned());
        }
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        Ok(Self { url })
    }

    /// The base URL, with a trailing slash.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The host, for a credential-free diagnostic.
    pub fn host(&self) -> &str {
        self.url.host_str().unwrap_or("localhost")
    }

    /// The scheme, which decides whether a redirect may stay on it.
    pub fn scheme(&self) -> &str {
        self.url.scheme()
    }

    /// Whether this origin is on the local filesystem.
    pub fn is_local(&self) -> bool {
        self.url.scheme() == "file"
    }

    /// Whether this origin is served over plain HTTP.
    ///
    /// True only for a loopback address. A remote plain-HTTP origin is refused
    /// at construction, so this is a statement about a test server rather than
    /// about a downgrade.
    pub fn is_loopback(&self) -> bool {
        is_loopback(&self.url)
    }

    /// Address one checked relative path on this origin.
    pub fn join(&self, relative: &RelativeContentPath) -> Result<Url, String> {
        self.url
            .join(relative.to_string().as_str())
            .map_err(|error| error.to_string())
    }

    /// The site an absolute URL belongs to, for a caller that has a whole URL
    /// rather than a path on a base.
    ///
    /// The result carries no path, so it is safe to use for a diagnostic, and it
    /// is the origin a credential would be bound to: a redirect that leaves this
    /// site does not receive one.
    pub fn site(url: &Url) -> Result<Self, String> {
        let mut site = url.clone();
        site.set_path("/");
        site.set_query(None);
        site.set_fragment(None);
        Self::from_url(site)
    }
}

/// Whether a URL addresses this machine.
///
/// Plain HTTP is permitted for exactly one case: a loopback address, which is a
/// test origin and a local development mirror. There is no traffic that leaves
/// the machine in that case, so there is nothing to intercept, and every other
/// origin must be HTTPS.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        // `localhost` resolves to loopback by definition on every platform zup
        // supports, and a name that resolves elsewhere would be a DNS answer
        // this crate has no business second-guessing in the other direction.
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The scheme and host only. A path could be a signed URL in some future
        // deployment, and a diagnostic is not the place that belongs.
        write!(formatter, "{}://{}", self.url.scheme(), self.host())
    }
}

/// A repository root and the origins that may serve it.
///
/// The first origin is preferred. The rest are fallbacks: a second CDN, an
/// enterprise mirror, a directory on the local network. Adding one is a
/// configuration change, not a code change, and none of them is trusted.
#[derive(Debug, Clone, Default)]
pub struct OriginSet {
    origins: Vec<Origin>,
    /// Which origin served the last success, purely so a caller can prefer it
    /// next time. This is a latency hint and nothing else.
    preferred: Arc<AtomicU64>,
}

impl OriginSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// An ordered set of origins.
    pub fn from_origins(origins: Vec<Origin>) -> Self {
        Self {
            origins,
            preferred: Arc::new(AtomicU64::new(u64::MAX)),
        }
    }

    /// Parse a primary repository URL and any mirrors.
    pub fn from_urls<'a>(
        primary: &str,
        mirrors: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, String> {
        let mut origins = vec![Origin::parse(primary)?];
        for mirror in mirrors {
            origins.push(Origin::parse(mirror)?);
        }
        Ok(Self::from_origins(origins))
    }

    /// Add an origin at the end of the preference order.
    pub fn push(&mut self, origin: Origin) {
        self.origins.push(origin);
    }

    /// How many origins are configured.
    pub fn len(&self) -> usize {
        self.origins.len()
    }

    /// Whether no origin is configured.
    pub fn is_empty(&self) -> bool {
        self.origins.is_empty()
    }

    /// The origins, in the order they should be tried.
    pub fn ordered(&self) -> Vec<&Origin> {
        let preferred = self.preferred.load(Ordering::Relaxed);
        let mut ordered: Vec<&Origin> = Vec::with_capacity(self.origins.len());
        if let Some(origin) = usize::try_from(preferred)
            .ok()
            .and_then(|index| self.origins.get(index))
        {
            ordered.push(origin);
        }
        ordered.extend(
            self.origins
                .iter()
                .enumerate()
                .filter(|(index, _)| *index as u64 != preferred)
                .map(|(_, origin)| origin),
        );
        ordered
    }

    /// The origins, in the order they will be tried, with their indices so a
    /// success can be recorded against the right one.
    pub fn indices(&self) -> Vec<(usize, &Origin)> {
        let preferred = self.preferred.load(Ordering::Relaxed);
        let mut ordered: Vec<(usize, &Origin)> = Vec::with_capacity(self.origins.len());
        if let Some(index) = usize::try_from(preferred).ok()
            && index < self.origins.len()
        {
            ordered.push((index, &self.origins[index]));
        }
        ordered.extend(
            self.origins
                .iter()
                .enumerate()
                .filter(|(index, _)| *index as u64 != preferred),
        );
        ordered
    }

    /// Note that an origin worked, so the next transfer tries it first.
    ///
    /// This is the only thing a fast mirror earns. It never changes what is
    /// verified.
    pub fn prefer(&self, index: usize) {
        self.preferred.store(index as u64, Ordering::Relaxed);
    }

    /// The origin at `index`.
    pub fn get(&self, index: usize) -> Option<&Origin> {
        self.origins.get(index)
    }
}

/// A repository's location: a channel, an origin set, and where the documents
/// live.
#[derive(Debug, Clone)]
pub struct RepositoryLocation {
    pub channel: String,
    pub origins: OriginSet,
}

impl RepositoryLocation {
    /// Build a location, checking the channel before anything is addressed.
    pub fn new(channel: &str, origins: OriginSet) -> Result<Self, String> {
        check_channel(channel)?;
        Ok(Self {
            channel: channel.to_owned(),
            origins,
        })
    }

    /// The release descriptor for this channel.
    pub fn release(&self) -> Result<RelativeContentPath, String> {
        zup_acquire::WebLayout::release(&self.channel).map_err(str::to_owned)
    }

    /// The content catalog for this channel.
    pub fn catalog(&self) -> Result<RelativeContentPath, String> {
        zup_acquire::WebLayout::catalog(&self.channel).map_err(str::to_owned)
    }

    /// One variant's manifest.
    pub fn variant_manifest(&self, variant: &str) -> Result<RelativeContentPath, String> {
        zup_acquire::WebLayout::variant_manifest(&self.channel, variant).map_err(str::to_owned)
    }
}
