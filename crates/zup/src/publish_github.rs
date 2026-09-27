//! GitHub as a release and content host, from the command line.
//!
//! # The two commands
//!
//! ```text
//! zup publish github          publish a release
//! zup ci github generate      write .github/workflows/release.yml
//! zup ci github check         say whether the committed workflow is current
//! ```
//!
//! Both derive everything they can from the project. `zup publish github` with
//! no arguments finds the repository, derives the tag from the version, measures
//! every file, and works out the rest from `zup-release.json` and the staged
//! tree. Every override exists for a case where the derivation is wrong, and
//! none of them is needed for the ordinary one.
//!
//! # What gets published
//!
//! ```text
//! Acme-Windows-Setup.exe        a person downloads this
//! zup-release.json              the release description
//! Acme-Windows-x64.zup          one variant's content, one asset
//! zup-package-win-x64.json      the small document that names it
//! zup-release-stable.json       the authenticated release descriptor
//! zup-catalog-stable.json       the authenticated content catalog
//! ```
//!
//! Never one asset per content object, and never a blob path: a release with ten
//! thousand assets is over the host's limit, unreadable, and a provider-specific
//! copy of a data model that belongs to the acquisition engine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_publish::{
    Application, HostLimits, ProductClass, ProductRole, ReleasePlan, ReleaseProduct, SourceClaim,
    TagIntent, TagPolicy,
};
use zup_publish_github::{
    GithubError, GithubRepository, LIMITS, MatrixTarget, NotesPolicy, ProcessEnvironment,
    PublishConfig, PublishRequest, RepositorySpec, WorkflowPolicy, find_git_config, publish,
};

/// Read the release manifest, or say what to run first.
fn read_release(release_dir: &Path) -> miette::Result<zup_artifact::ReleaseManifest> {
    let path = release_dir.join(zup_artifact::RELEASE_MANIFEST_NAME);
    let bytes = std::fs::read(&path).map_err(|error| {
        miette::miette!(
            "`{}` could not be read: {error}; run `zup build` before publishing",
            path.display()
        )
    })?;
    zup_artifact::ReleaseManifest::parse(&bytes).map_err(|error| {
        miette::miette!("`{}` is not a release description: {error}", path.display())
    })
}

/// Where the files a build produced are.
#[derive(Debug, Clone, Default)]
pub struct Staged {
    /// The directory `zup build` wrote its release description into.
    pub release_dir: PathBuf,
    /// The staged content tree, for the documents a client authenticates.
    pub web: Option<PathBuf>,
    /// The transport package directory, for GitHub-hosted content.
    pub packages: Option<PathBuf>,
}

/// How one publication should behave.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dry_run: bool,
    pub draft: Option<bool>,
    pub prerelease: Option<bool>,
    pub receipt: Option<PathBuf>,
    pub notes_text: Option<String>,
}

/// Build the release plan from what a build produced.
///
/// Three sources, and the roles are what keep them apart: the release manifest
/// names the files a person downloads, the staged web tree names the documents a
/// client authenticates, and the package directory names the transport objects.
/// A file that belongs to two roles is uploaded once, and the plan's validation
/// refuses a name that appears in two roles with different bytes — which is the
/// one way this could produce a release nobody can verify.
pub fn build_plan(
    manifest: &zup_manifest::Manifest,
    config: &PublishConfig,
    release: &zup_artifact::ReleaseManifest,
    staged: &Staged,
    tag: &TagIntent,
    limits: &HostLimits,
) -> miette::Result<ReleasePlan> {
    let release_dir = staged.release_dir.as_path();
    let web = staged.web.as_deref();
    let packages = staged.packages.as_deref();
    let mut plan = ReleasePlan::new(
        Application::new(
            release.application.id.clone(),
            release.application.name.as_str(),
            release.application.version.to_string(),
        ),
        tag.clone(),
    )
    .with_source(SourceClaim::none());

    for artifact in &release.artifacts {
        let path = join_release(release_dir, &artifact.path)?;
        let product = ReleaseProduct::new(
            asset_name(&artifact.path)?,
            ProductRole::Install,
            ProductClass::UserFacing,
            digest_of(&path)?,
            artifact.size,
        )
        .with_media_type(media_type_for(&artifact.path))
        .serving(std::iter::once(artifact.id.clone()));
        plan.push(product);
    }

    // The release description is itself a release product. A consumer who wants
    // to know what a release contains before downloading 400 MiB of it needs a
    // way to fetch exactly that, and the portable manifest is the document.
    let manifest_path = release_dir.join(zup_artifact::RELEASE_MANIFEST_NAME);
    plan.push(
        ReleaseProduct::new(
            zup_artifact::RELEASE_MANIFEST_NAME,
            ProductRole::Manifest,
            ProductClass::UserFacing,
            digest_of(&manifest_path)?,
            size_of(&manifest_path)?,
        )
        .with_media_type("application/json")
        .serving(["release".to_owned()]),
    );

    if let Some(web) = web
        && web.is_dir()
    {
        for (document, path) in documents(web)? {
            plan.push(
                ReleaseProduct::new(
                    document,
                    ProductRole::Manifest,
                    ProductClass::Transport,
                    digest_of(&path)?,
                    size_of(&path)?,
                )
                .with_media_type("application/json"),
            );
        }
    }

    if let Some(packages) = packages
        && packages.is_dir()
    {
        for (document, path) in package_documents(packages)? {
            plan.push(
                ReleaseProduct::new(
                    document,
                    ProductRole::Manifest,
                    ProductClass::Transport,
                    digest_of(&path)?,
                    size_of(&path)?,
                )
                .with_media_type("application/json"),
            );
        }
        for (name, path) in package_files(packages)? {
            plan.push(
                ReleaseProduct::new(
                    name,
                    // A transport object, not something a person downloads, which
                    // is exactly why the host may split it to fit a size limit.
                    ProductRole::Auxiliary,
                    ProductClass::Transport,
                    digest_of(&path)?,
                    size_of(&path)?,
                )
                .with_media_type("application/vnd.zup.package.v1"),
            );
        }
    }

    if let Some(origin) = content_origin(manifest, config) {
        plan = plan.with_origins(vec![origin]);
    }
    // Preflight before anything is written, so a release that cannot be published
    // is refused before a draft exists rather than after eleven uploads.
    plan.preflight(limits)
        .map_err(|error| miette::miette!("{error}"))?;
    Ok(plan)
}

/// The content origin a project's distribution host implies, if it has one.
fn content_origin(
    _manifest: &zup_manifest::Manifest,
    config: &PublishConfig,
) -> Option<zup_publish::ContentOrigin> {
    let repository = config.repository.as_ref()?;
    let host = config.install();
    let path = format!("{}/{}", repository.owner, repository.name);
    let base = host.latest_download_url(&path, "");
    Some(zup_publish_github::content_origin(
        config.distribution,
        base,
    ))
}

/// Every release document in a staged web tree, as `(asset name, path)`.
///
/// Sorted, because a release plan is a document and a document whose order
/// depends on a directory listing's order is a document whose digest changes for
/// no reason.
fn documents(web: &Path) -> miette::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    collect_documents(web, web, &mut out)?;
    out.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(out)
}

fn collect_documents(
    root: &Path,
    directory: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> miette::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(miette::miette!("`{}`: {error}", directory.display()));
        }
    };
    for entry in entries {
        let path = entry
            .map_err(|error| miette::miette!("`{}`: {error}", directory.display()))?
            .path();
        if path.is_dir() {
            collect_documents(root, &path, out)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| miette::miette!("`{}` is not under the staged tree", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        // A blob is content, not a document. Publishing one file per blob is the
        // failure this whole design exists to prevent, so it is refused at the
        // point where it would otherwise happen.
        if relative.starts_with("blobs/") {
            continue;
        }
        let Some(parsed) = relative.parse::<zup_acquire::RelativeContentPath>().ok() else {
            continue;
        };
        let name =
            zup_publish::asset_name(&parsed).map_err(|reason| miette::miette!("{reason}"))?;
        out.push((name, path));
    }
    Ok(())
}

/// Every package descriptor in a package directory, as `(asset name, path)`.
fn package_documents(packages: &Path) -> miette::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for (name, path) in package_files(packages)? {
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let variant = stem
            .rsplit_once('-')
            .map(|(_, variant)| variant)
            .unwrap_or(stem);
        out.push((
            zup_publish::document_name(&zup_publish::DocumentKind::Package { variant }),
            path,
        ));
    }
    out.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(out)
}

/// Every transport package in a package directory, as `(asset name, path)`.
fn package_files(packages: &Path) -> miette::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(packages)
        .map_err(|error| miette::miette!("`{}`: {error}", packages.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|error| miette::miette!("`{}`: {error}", packages.display()))?
            .path();
        if !path.is_file() {
            continue;
        }
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| miette::miette!("`{}` has no file name", path.display()))?;
        if !file.ends_with(".zup") {
            continue;
        }
        // A sharded package's pieces are already conservatively named by the
        // writer; the name check is the guarantee that publication identity never
        // depends on the host not normalising a filename.
        zup_publish::check_asset_name(file).map_err(|reason| {
            miette::miette!("`{file}` cannot be a release asset name: {reason}")
        })?;
        out.push((file.to_owned(), path));
    }
    out.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(out)
}

/// Resolve the tag a release is published under.
pub fn resolve_tag(
    version: &str,
    config: &PublishConfig,
    override_tag: Option<&str>,
) -> miette::Result<TagIntent> {
    if let Some(tag) = override_tag {
        if tag.trim().is_empty() {
            return Err(miette::miette!("`--tag` was given an empty tag"));
        }
        return Ok(if config.create_tag {
            TagIntent::creatable(
                tag.trim(),
                TagPolicy::Exact {
                    tag: tag.trim().into(),
                },
            )
        } else {
            TagIntent::required(tag.trim())
        });
    }
    if let Some(tag) = &config.tag {
        return Ok(TagIntent::required(tag));
    }
    let policy = config.tag_policy();
    let tag = policy
        .derive(version)
        .map_err(|error| miette::miette!("{error}"))?;
    Ok(if config.create_tag {
        TagIntent::creatable(tag, policy)
    } else {
        TagIntent::required(tag)
    })
}

/// Find the repository this release belongs to.
pub fn resolve_repository(
    config: &PublishConfig,
    explicit: Option<&str>,
    working_directory: &Path,
) -> miette::Result<(GithubRepository, String)> {
    let spec = match explicit {
        Some(value) => Some(RepositorySpec::parse(value).map_err(failed)?),
        None => config.repository.clone(),
    };
    let environment = ProcessEnvironment;
    let root = find_git_config(working_directory)
        .and_then(|git| git.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| working_directory.to_path_buf());
    let resolved =
        zup_publish_github::resolve(spec.as_ref(), &environment, &root).map_err(failed)?;
    Ok((
        resolved.repository,
        format!(
            "{} ({})",
            resolved.discovery.as_str(),
            resolved
                .remote
                .as_deref()
                .map(|remote| format!("remote `{remote}`"))
                .unwrap_or_default()
        ),
    ))
}

fn failed(error: GithubError) -> miette::Report {
    miette::miette!("{error}")
}

/// Run one publication.
pub fn run(
    manifest: &zup_manifest::Manifest,
    config: &PublishConfig,
    staged: &Staged,
    repository: &GithubRepository,
    token: &zup_publish_github::Token,
    options: &Options,
) -> miette::Result<zup_publish::PublishReport> {
    let release_dir = staged.release_dir.as_path();
    let web = staged.web.as_deref();
    let packages = staged.packages.as_deref();
    let release = read_release(release_dir)?;
    let tag = resolve_tag(&release.application.version.to_string(), config, None)?;
    let plan = build_plan(manifest, config, &release, staged, &tag, &LIMITS)?;
    let sources = locate_sources(&plan, release_dir, web, packages)?;

    let mut request = PublishRequest::new(plan);
    request.sources = sources;
    request.notes = config.notes.clone();
    request.notes_file = match &config.notes {
        NotesPolicy::File(path) => Some(PathBuf::from(path)),
        _ => None,
    };
    request.notes_text = options
        .notes_text
        .clone()
        .or_else(|| config.notes_text.clone());
    request.dry_run = options.dry_run;
    request.draft = options.draft.unwrap_or(config.draft);
    request.prerelease = options.prerelease.unwrap_or(config.prerelease);
    request.replace_conflicts = config.replace_conflicts;
    request.receipt = options.receipt.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("a runtime for the publish request: {error}"))?;
    runtime
        .block_on(publish(repository, token, &request))
        .map_err(failed)
}

/// Match each product to the file that holds it.
fn locate_sources(
    plan: &ReleasePlan,
    release_dir: &Path,
    web: Option<&Path>,
    packages: Option<&Path>,
) -> miette::Result<BTreeMap<String, PathBuf>> {
    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    for role in ProductRole::ALL {
        for product in plan.role(role) {
            if by_name.contains_key(&product.name) {
                continue;
            }
            let path = match product.name.as_str() {
                zup_artifact::RELEASE_MANIFEST_NAME => {
                    release_dir.join(zup_artifact::RELEASE_MANIFEST_NAME)
                }
                name if name.ends_with(".zup") || name.contains(".zup.") => {
                    let packages = packages.ok_or_else(|| {
                        miette::miette!(
                            "`{name}` is a transport package but no package directory was given"
                        )
                    })?;
                    packages.join(name)
                }
                name => {
                    // A document, or an installer beside the release manifest.
                    let mut candidates = vec![release_dir.join(name)];
                    if let Some(web) = web {
                        candidates.push(web.join(name));
                    }
                    match candidates.into_iter().find(|path| path.is_file()) {
                        Some(found) => found,
                        // Flat asset names are not tree paths, so the staged tree
                        // is searched by the name the release will carry rather
                        // than by the name it has on disk.
                        None => search_tree(web, name, &product.name)?,
                    }
                }
            };
            if !path.is_file() {
                return Err(miette::miette!(
                    "the release plan names `{}` but `{}` does not exist",
                    product.name,
                    path.display()
                ));
            }
            by_name.insert(product.name.clone(), path);
        }
    }
    Ok(by_name)
}

/// Find a document by the flat name it will be published under.
fn search_tree(web: Option<&Path>, asset: &str, original: &str) -> miette::Result<PathBuf> {
    let Some(web) = web else {
        return Err(miette::miette!(
            "the release plan names `{original}`, which is not in the release directory"
        ));
    };
    let mut found = None;
    let mut stack = vec![web.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if found.is_some() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(web) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative.starts_with("blobs/") {
                continue;
            }
            let Ok(parsed) = relative.parse::<zup_acquire::RelativeContentPath>() else {
                continue;
            };
            if zup_publish::asset_name(&parsed).ok().as_deref() == Some(asset) {
                found = Some(path);
            }
        }
    }
    found.ok_or_else(|| {
        miette::miette!("the release plan names `{original}`, which is not in the staged tree")
    })
}

/// The matrix a generated workflow builds, from the manifest's own profiles.
pub fn matrix(manifest: &zup_manifest::Manifest) -> Vec<MatrixTarget> {
    manifest
        .build
        .targets
        .iter()
        .map(|(profile, target)| MatrixTarget::new(profile.as_str(), target.target.as_str()))
        .collect()
}

/// Write the release workflow, or say whether the committed one is current.
pub fn workflow(
    root: &Path,
    manifest: &zup_manifest::Manifest,
    config: &PublishConfig,
) -> miette::Result<zup_publish_github::Freshness> {
    let policy: &WorkflowPolicy = &config.workflow;
    let targets = matrix(manifest);
    Ok(zup_publish_github::check(root, policy, &targets))
}

/// Render the generated workflow.
pub fn render_workflow(manifest: &zup_manifest::Manifest, config: &PublishConfig) -> String {
    zup_publish_github::generate(&config.workflow, &matrix(manifest))
}

/// The tag a release would use, for a report.
///
/// Falls back to the bare version only for a manifest with no version, which the
/// manifest schema already refuses; a tag policy that cannot be spelled is a
/// refusal at parse time, so it cannot reach here.
pub fn preview_tag(manifest: &zup_manifest::Manifest, config: &PublishConfig) -> String {
    resolve_tag(&manifest.app.version.to_string(), config, None)
        .map(|tag| tag.tag)
        .unwrap_or_else(|_| manifest.app.version.to_string())
}

fn asset_name(path: &str) -> miette::Result<String> {
    let trimmed = path.trim_start_matches("./");
    if let Ok(parsed) = trimmed.parse::<zup_acquire::RelativeContentPath>()
        && let Ok(name) = zup_publish::asset_name(&parsed)
    {
        return Ok(name);
    }
    let name = trimmed.replace('\\', "/").replace('/', "-");
    zup_publish::check_asset_name(&name)
        .map_err(|reason| miette::miette!("`{path}` cannot be a release asset name: {reason}"))?;
    Ok(name)
}

fn media_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("exe") => "application/vnd.microsoft.portable-executable",
        Some("msi") => "application/x-msi",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

fn join_release(root: &Path, relative: &str) -> miette::Result<PathBuf> {
    let relative = relative.replace('\\', "/");
    if relative.starts_with('/') || relative.contains("..") || relative.contains(':') {
        return Err(miette::miette!(
            "`{relative}` is not a path inside the release root"
        ));
    }
    Ok(root.join(relative))
}

fn digest_of(path: &Path) -> miette::Result<zup_core::Sha256Digest> {
    let file = std::fs::File::open(path)
        .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?;
    zup_core::hash_reader(std::io::BufReader::new(file))
        .map(|(_, digest)| digest)
        .map_err(|error| miette::miette!("`{}`: {error}", path.display()))
}

fn size_of(path: &Path) -> miette::Result<u64> {
    std::fs::metadata(path)
        .map(|meta| meta.len())
        .map_err(|error| miette::miette!("`{}`: {error}", path.display()))
}
