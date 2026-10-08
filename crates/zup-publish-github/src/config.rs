use std::path::PathBuf;

use crate::{GithubHost, MatrixTarget, NotesPolicy, RepositorySpec, Signing, WorkflowPolicy};

use zup_manifest::ManifestError;
use zup_manifest::{DistributionHost, Github, GithubNotes, GithubWorkflow, Manifest};

#[derive(Debug, Clone)]
pub struct PublishConfig {
    pub repository: Option<RepositorySpec>,
    pub tag_prefix: Option<String>,
    pub tag: Option<String>,
    pub create_tag: bool,
    pub replace_conflicts: bool,
    pub draft: bool,
    pub prerelease: bool,
    pub distribution: DistributionHost,
    pub notes: NotesPolicy,
    pub notes_file: Option<PathBuf>,
    pub notes_text: Option<String>,
    pub workflow: WorkflowPolicy,
    pub configured: bool,
}

impl PublishConfig {
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

    pub fn github(manifest: &Manifest) -> Option<&Github> {
        manifest
            .publish
            .as_ref()
            .and_then(|publish| publish.github.as_ref())
    }

    pub fn resolve(manifest: &Manifest) -> Result<Self, ManifestError> {
        let mut config = Self::empty();
        config.distribution = manifest
            .distribution
            .as_ref()
            .map_or(DistributionHost::Static, |distribution| distribution.host);
        let Some(github) = Self::github(manifest) else {
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

    pub fn tag_policy(&self) -> zup_publish::TagPolicy {
        match &self.tag {
            Some(tag) => zup_publish::TagPolicy::Exact { tag: tag.clone() },
            None => zup_publish::TagPolicy::Versioned {
                prefix: self.tag_prefix.clone().unwrap_or_else(|| "v".to_owned()),
            },
        }
    }

    fn check_tag_policy(&self) -> Result<(), ManifestError> {
        if self.tag.as_deref().is_some_and(str::is_empty) {
            return Err(ManifestError::Invalid {
                message: "`[publish.github.tag] name` is empty; a tag has to name something"
                    .to_owned(),
                src: None,
                span: None,
            });
        }
        self.tag_policy()
            .derive("0")
            .map(|_| ())
            .map_err(|error| ManifestError::Invalid {
                message: format!("`[publish.github.tag]` cannot spell a tag: {error}"),
                src: None,
                span: None,
            })
    }

    pub fn install(&self) -> GithubHost {
        self.repository
            .as_ref()
            .and_then(|spec| spec.host.clone())
            .unwrap_or_else(GithubHost::dotcom)
    }

    pub fn matrix(&self, manifest: &Manifest) -> Vec<MatrixTarget> {
        manifest
            .build
            .targets
            .iter()
            .map(|(profile, target)| MatrixTarget::new(profile.as_str(), target.target.as_str()))
            .collect()
    }
}

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
    if let Some(action) = &workflow.action {
        policy.action = action.clone();
    }
    policy.runner_overrides = workflow.runners.clone();
    Ok(policy)
}

pub fn content_origin(distribution: DistributionHost, base: String) -> zup_publish::ContentOrigin {
    let kind = match distribution {
        DistributionHost::Static => zup_publish::OriginKind::Static,
        DistributionHost::Github => zup_publish::OriginKind::Release,
    };
    zup_publish::ContentOrigin::new(kind, base)
}
