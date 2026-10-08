use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum, ValueHint};

use crate::publish;

#[derive(Debug, Args)]
pub struct CiCommand {
    #[command(subcommand)]
    command: CiSubcommand,
}

#[derive(Debug, Subcommand)]
enum CiSubcommand {
    Github(CiGithubCommand),
}

#[derive(Debug, Args)]
struct CiGithubCommand {
    #[command(subcommand)]
    command: CiGithubSubcommand,
}

#[derive(Debug, Subcommand)]
enum CiGithubSubcommand {
    Generate(CiGithubGenerateCommand),
    Check(CiGithubCheckCommand),
}

#[derive(Debug, Args)]
struct CiGithubGenerateCommand {
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    manifest: PathBuf,
    #[arg(long, value_hint = ValueHint::FilePath)]
    output: Option<PathBuf>,
    #[arg(long)]
    stdout: bool,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Args)]
struct CiGithubCheckCommand {
    #[arg(long, default_value = crate::DEFAULT_MANIFEST, value_hint = ValueHint::FilePath)]
    manifest: PathBuf,
    #[arg(long, value_enum, default_value = "human")]
    format: FormatArg,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum FormatArg {
    #[default]
    Human,
    Json,
}

impl FormatArg {
    const fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

pub fn run(args: CiCommand) -> miette::Result<()> {
    match args.command {
        CiSubcommand::Github(args) => match args.command {
            CiGithubSubcommand::Generate(args) => generate(args),
            CiGithubSubcommand::Check(args) => check(args),
        },
    }
}

fn load(path: &Path) -> miette::Result<zup_manifest::Manifest> {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let source = std::fs::read_to_string(&absolute)
        .map_err(|error| miette::miette!("`{}`: {error}", absolute.display()))?;
    zup_manifest::parse_named(&source, &absolute.display().to_string())
        .map_err(|error| miette::miette!("{error}"))
}

fn root_of(manifest: &Path) -> PathBuf {
    manifest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn generate(args: CiGithubGenerateCommand) -> miette::Result<()> {
    let manifest = load(&args.manifest)?;
    let config = zup_publish_github::PublishConfig::resolve(&manifest)?;
    let rendered = publish::render_workflow(&manifest, &config);
    let root = root_of(&args.manifest);
    let path = args
        .output
        .clone()
        .unwrap_or_else(|| root.join(zup_publish_github::WORKFLOW_PATH));
    if args.stdout {
        print!("{rendered}");
        return Ok(());
    }
    if args.check {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing == rendered {
            println!("{} is current", path.display());
            return Ok(());
        }
        return Err(miette::miette!(
            "{} does not match the generator; run `zup ci github generate` and review the diff",
            path.display()
        ));
    }
    if path.exists() && !args.force {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing == rendered {
            println!("{} is already current", path.display());
            return Ok(());
        }
        return Err(miette::miette!(
            "{} exists and differs from the generator; pass --force to replace it, after \
             reading the diff",
            path.display()
        ));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| miette::miette!("`{}`: {error}", parent.display()))?;
    }
    std::fs::write(&path, &rendered)
        .map_err(|error| miette::miette!("`{}`: {error}", path.display()))?;
    println!("Wrote {}", path.display());
    println!("  Targets     {}", publish::matrix(&manifest).len());
    println!("  Tag         {}", publish::preview_tag(&manifest, &config));
    println!(
        "  Attest      {}",
        if config.workflow.attestations {
            "on"
        } else {
            "off"
        }
    );
    println!(
        "  Environment {}",
        config.workflow.environment.as_deref().unwrap_or("none")
    );
    Ok(())
}

fn check(args: CiGithubCheckCommand) -> miette::Result<()> {
    let manifest = load(&args.manifest)?;
    let config = zup_publish_github::PublishConfig::resolve(&manifest)?;
    let root = root_of(&args.manifest);
    let freshness = publish::workflow(&root, &manifest, &config)?;
    let report = Report {
        version: REPORT_VERSION,
        path: freshness.path.clone(),
        present: freshness.present,
        current: freshness.current,
        detail: freshness.detail.clone(),
        tag: publish::preview_tag(&manifest, &config),
        profiles: publish::matrix(&manifest)
            .iter()
            .map(|target| {
                let native = zup_publish_github::native_runner(&target.triple);
                Profile {
                    profile: target.profile.clone(),
                    target: target.triple.clone(),
                    runner: native
                        .map(|runner| runner.label().to_owned())
                        .unwrap_or_else(|| "cross-compiled".to_owned()),
                    native: native.is_some_and(|runner| runner.native),
                }
            })
            .collect(),
        attestations: config.workflow.attestations,
        environment: config.workflow.environment.clone(),
        signing: config.workflow.signing.is_some(),
        actions: zup_publish_github::pins()
            .iter()
            .map(|pin| Action {
                repository: pin.repository.clone(),
                version: pin.version.clone(),
                sha: pin.sha.clone(),
                checked_at: pin.checked_at.clone(),
                in_generated_workflow: zup_publish_github::generated_actions()
                    .contains(&pin.repository.as_str()),
            })
            .collect(),
    };
    if args.format.is_json() {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|error| miette::miette!("report: {error}"))?;
        println!("{json}");
    } else {
        print!("{}", report.human());
    }
    if !report.current {
        return Err(miette::miette!("{}", report.detail));
    }
    Ok(())
}

const REPORT_VERSION: u32 = 1;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct Action {
    repository: String,
    version: String,
    sha: String,
    checked_at: String,
    in_generated_workflow: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct Profile {
    profile: String,
    target: String,
    runner: String,
    native: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct Report {
    version: u32,
    path: String,
    present: bool,
    current: bool,
    detail: String,
    tag: String,
    profiles: Vec<Profile>,
    attestations: bool,
    environment: Option<String>,
    signing: bool,
    actions: Vec<Action>,
}

impl Report {
    fn human(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("{:<16}{}\n", "Workflow", self.path));
        out.push_str(&format!(
            "{:<16}{}\n",
            "State",
            if self.current { "current" } else { "stale" }
        ));
        out.push_str(&format!("{:<16}{}\n", "Tag", self.tag));
        out.push_str(&format!(
            "{:<16}{}\n",
            "Attestations",
            on_off(self.attestations)
        ));
        out.push_str(&format!("{:<16}{}\n", "Signing", on_off(self.signing)));
        out.push_str(&format!(
            "{:<16}{}\n",
            "Environment",
            self.environment.as_deref().unwrap_or("none")
        ));
        out.push('\n');
        out.push_str("Target matrix\n");
        for profile in &self.profiles {
            out.push_str(&format!(
                "  {:<18} {:<28} {}{}\n",
                profile.profile,
                profile.target,
                profile.runner,
                if profile.native { "" } else { "  (cross)" }
            ));
        }
        out.push('\n');
        let used: Vec<&Action> = self
            .actions
            .iter()
            .filter(|action| action.in_generated_workflow)
            .collect();
        out.push_str("Action refs\n");
        for action in &used {
            out.push_str(&format!(
                "  {:<32} {:<10} {}  (checked {})\n",
                action.repository, action.version, action.sha, action.checked_at
            ));
        }
        out.push('\n');
        out.push_str(&self.detail);
        out.push('\n');
        out
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}
