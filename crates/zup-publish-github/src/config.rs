//! Turning the manifest's publish settings into provider configuration.
//!
//! This is the seam between "what a project declared" and "what a provider
//! needs", and it is deliberately the only place the two meet.
//!
//! `zup-manifest` owns the *shape* of a `[publish.github]` table and knows
//! nothing about a release id, an API, or a credential — the same way it owns the
//! shape of `[build.artifacts]` and knows nothing about a PE. `zup-publish-github`
//! owns the *meaning* and reads these types. Neither owns the other half, and
//! the manifest cannot be extended to name a provider the CLI does not support,
//! which is what stops a project from configuring a release surface that does
//! not exist.
//!
//! Every field with a sensible default is optional here, and every field without
//! one is a hard error rather than a fallback. A workflow that quietly publishes
//! without signing because a key was not configured is worse than a workflow that
//! refuses to generate.

use std::path::PathBuf;

use crate::{GithubHost, MatrixTarget, NotesPolicy, RepositorySpec, Signing, WorkflowPolicy};

use zup_manifest::ManifestError;
use zup_manifest::{DistributionHost, Github, GithubNotes, GithubWorkflow, Manifest};

/// Everything a publish command needs, resolved from the manifest.
#[derive(Debug, Clone)]
pub struct PublishConfig {
    /// The repository, as the project named it.
    pub repository: Option<RepositorySpec>,
    /// The tag prefix, when the project chose one.
    pub tag_prefix: Option<String>,
    /// An exact tag, when the project chose one.
    pub tag: Option<String>,
    /// Whether the publisher may create the tag.
    pub create_tag: bool,
    /// Whether a differing draft asset may be replaced.
    pub replace_conflicts: bool,
    /// Whether to leave the release a draft.
    pub draft: bool,
    /// Whether to mark the release a prerelease.
    pub prerelease: bool,
    /// Where a client fetches content from.
    pub distribution: DistributionHost,
    /// The notes policy.
    pub notes: NotesPolicy,
    /// The file to read for the `file` policy.
    pub notes_file: Option<PathBuf>,
    /// Text for the `text` policy.
    pub notes_text: Option<String>,
    /// The generated workflow's settings.
    pub workflow: WorkflowPolicy,
    /// Whether the project declared a `[publish.github]` table at all.
    pub configured: bool,
}

impl PublishConfig {
    /// A configuration with nothing declared.
    pub fn empty() -> Self {
        Self {
            repository: None,
            tag_prefix: None,
            tag: None,
            create_tag: false,
            replace_conflicts: false,
            draft: false,
            prerelease: false,
            distribution: DistributionHost::Static,
            notes: NotesPolicy::default(),
            notes_file: None,
            notes_text: None,
            workflow: WorkflowPolicy::default(),
            configured: false,
        }
    }

    /// The GitHub table, if the manifest has one.
    pub fn github(manifest: &Manifest) -> Option<&Github> {
        manifest
            .publish
            .as_ref()
            .and_then(|publish| publish.github.as_ref())
    }

    /// Resolve a manifest's publish settings.
    pub fn resolve(manifest: &Manifest) -> Result<Self, ManifestError> {
        let mut config = Self::empty();
        config.distribution = manifest
            .distribution
            .as_ref()
            .map_or(DistributionHost::Static, |distribution| distribution.host);
        let Some(github) = Self::github(manifest) else {
            // `[distribution] host = "github"` with no repository is a project
            // that wants zero-infrastructure distribution and has not said where
            // the release lives. That is worth catching at parse time rather than
            // at the end of a build.
            if config.distribution == DistributionHost::Github {
                return Err(ManifestError::Invalid {
                    message: "`[distribution] host = \"github\"` needs a repository; add \
                              `[publish.github] repository = \"owner/name\"`"
                        .to_owned(),
                    src: None,
                    span: None,
                });
            }
            return Ok(config);
        };
        config.configured = true;
        config.repository = Some(RepositorySpec::parse(&github.repository).map_err(|error| {
            ManifestError::Invalid {
                message: format!("`[publish.github] repository` is not usable: {error}"),
                src: None,
                span: None,
            }
        })?);
        config.draft = github.draft.unwrap_or(false);
        config.prerelease = github.prerelease.unwrap_or(false);
        config.create_tag = github.create_tag.unwrap_or(false);
        config.replace_conflicts = github.replace_conflicts.unwrap_or(false);
        if let Some(tag) = &github.tag {
            config.tag_prefix = tag.prefix.clone();
            config.tag = tag.name.clone();
            if config.tag.is_some() && config.tag_prefix.is_some() {
                return Err(ManifestError::Invalid {
                    message: "`[publish.github.tag]` takes `prefix` or `name`, not both: a tag \
                              is either derived from the version or chosen outright"
                        .to_owned(),
                    src: None,
                    span: None,
                });
            }
        }
        // The tag policy is checked here rather than at publication, so every
        // entry point refuses a tag it could never spell: a manifest that only
        // fails once eleven gigabytes have been uploaded is a manifest that gets
        // fixed the next time somebody tries to release.
        config.check_tag_policy()?;
        config.notes = notes_policy(github.notes.as_ref())?;
        config
            .notes_file
            .clone_from(&github.notes.as_ref().and_then(|notes| notes.file.clone()));
        config
            .notes_text
            .clone_from(&github.notes.as_ref().and_then(|notes| notes.text.clone()));
        config.workflow = workflow_policy(github.workflow.as_ref())?;
        Ok(config)
    }

    /// The tag this configuration would publish `version` under.
    pub fn tag_policy(&self) -> zup_publish::TagPolicy {
        match &self.tag {
            Some(tag) => zup_publish::TagPolicy::Exact { tag: tag.clone() },
            None => zup_publish::TagPolicy::Versioned {
                prefix: self.tag_prefix.clone().unwrap_or_else(|| "v".to_owned()),
            },
        }
    }

    /// Whether the configured tag could ever be spelled.
    ///
    /// Checked while the manifest is being read rather than when a release is
    /// published: a tag prefix is typed once and used for every release, so a
    /// bad one is a typo to be caught by the editor, not by a failed upload.
    fn check_tag_policy(&self) -> Result<(), ManifestError> {
        if self.tag.as_deref().is_some_and(str::is_empty) {
            return Err(ManifestError::Invalid {
                message: "`[publish.github.tag] name` is empty; a tag has to name something"
                    .to_owned(),
                src: None,
                span: None,
            });
        }
        // `derive` checks the prefix, not the version, so the version here only
        // has to be a string the prefix could be joined to.
        self.tag_policy()
            .derive("0")
            .map(|_| ())
            .map_err(|error| ManifestError::Invalid {
                message: format!("`[publish.github.tag]` cannot spell a tag: {error}"),
                src: None,
                span: None,
            })
    }

    /// The installation this configuration targets, defaulting to github.com.
    pub fn install(&self) -> GithubHost {
        self.repository
            .as_ref()
            .and_then(|spec| spec.host.clone())
            .unwrap_or_else(GithubHost::dotcom)
    }

    /// The matrix the generated workflow builds.
    pub fn matrix(&self, manifest: &Manifest) -> Vec<MatrixTarget> {
        manifest
            .build
            .targets
            .iter()
            .map(|(profile, target)| MatrixTarget::new(profile.as_str(), target.target.as_str()))
            .collect()
    }
}

/// Resolve a notes policy, and check that the policy's own field is present.
///
/// The check is the point. A `file` policy with no `file` and a `text` policy
/// with no `text` are both silent no-ops if they are allowed through, and a
/// release with no notes is a worse outcome than a manifest that will not parse.
fn notes_policy(notes: Option<&GithubNotes>) -> Result<NotesPolicy, ManifestError> {
    let Some(notes) = notes else {
        return Ok(NotesPolicy::default());
    };
    match notes.policy.as_str() {
        "generated" | "github" => Ok(NotesPolicy::Generated),
        "none" => Ok(NotesPolicy::None),
        "file" => {
            let Some(file) = notes.file.clone() else {
                return Err(ManifestError::Invalid {
                    message: "`notes = \"file\"` needs a `file`".to_owned(),
                    src: None,
                    span: None,
                });
            };
            Ok(NotesPolicy::File(file.display().to_string()))
        }
        "text" => {
            let Some(text) = notes.text.clone() else {
                return Err(ManifestError::Invalid {
                    message: "`notes = \"text\"` needs a `text`".to_owned(),
                    src: None,
                    span: None,
                });
            };
            Ok(NotesPolicy::Text(text))
        }
        other => Err(ManifestError::Invalid {
            message: format!(
                "`notes` must be one of `generated`, `file`, `text`, `none`; found `{other}`"
            ),
            src: None,
            span: None,
        }),
    }
}

/// Resolve the generated workflow's settings.
fn workflow_policy(workflow: Option<&GithubWorkflow>) -> Result<WorkflowPolicy, ManifestError> {
    let mut policy = WorkflowPolicy::default();
    let Some(workflow) = workflow else {
        return Ok(policy);
    };
    if let Some(name) = &workflow.name {
        policy.name = name.clone();
    }
    if let Some(glob) = &workflow.tag_glob {
        policy.tag_glob = glob.clone();
    }
    policy.environment.clone_from(&workflow.environment);
    if let Some(attestations) = workflow.attestations {
        policy.attestations = attestations;
    }
    if let Some(paths) = &workflow.attest_paths {
        policy.attest_paths.clone_from(paths);
    }
    if let Some(commands) = &workflow.sign
        && !commands.is_empty()
    {
        policy.signing = Some(Signing {
            command: commands.join("\n"),
        });
    }
    if let Some(runner) = &workflow.compose_runner {
        policy.compose_runner = runner.clone();
    }
    if let Some(dir) = &workflow.release_dir {
        policy.release_dir = dir.clone();
    }
    if let Some(receipt) = &workflow.receipt {
        policy.receipt = receipt.clone();
    }
    policy.runner_overrides = workflow.runners.clone();
    Ok(policy)
}

/// The content origin a distribution host implies, for a release plan.
///
/// `github` becomes a *release* origin rather than a static one, because the
/// shape is different: assets are addressed by name under a release rather than
/// by path under a directory. The distinction matters to a client that has to
/// build a URL, and it is why the two are different variants rather than one
/// host with a flag.
pub fn content_origin(distribution: DistributionHost, base: String) -> zup_publish::ContentOrigin {
    let kind = match distribution {
        DistributionHost::Static => zup_publish::OriginKind::Static,
        DistributionHost::Github => zup_publish::OriginKind::Release,
    };
    zup_publish::ContentOrigin::new(kind, base)
}
