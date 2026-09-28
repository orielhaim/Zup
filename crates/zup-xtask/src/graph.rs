//! The dependency-graph gate.
//!
//! `cargo deny` reports duplicate versions and this module refuses the ones that
//! are zup's fault. The distinction matters: the graph legitimately contains 49
//! duplicate versions forced by upstream crates zup does not control, and a gate
//! that refused all 49 would be a gate that only tests its own exception list.
//!
//! What this refuses is narrower and much sharper:
//!
//! - **A workspace crate that depends on two versions of one external crate.**
//!   That is never upstream's doing: it means somebody added a dependency with a
//!   looser requirement to a package that already had a tighter one, and from
//!   then on a fix lands in one copy and not the other.
//! - **A crate in a graph that may not contain it.** The example that matters is
//!   `zup-publish-github`: it is a *developer* tool, and the day it reaches
//!   `zup-installer` through a shared dependency, every user of an Acme installer
//!   ships a GitHub API client they did not ask for. That is a real production
//!   defect, and it is invisible to a size budget until somebody looks.
//!
//! Both are checked by reading Cargo metadata, so they run offline and take
//! milliseconds, and both fail with the offending edge rather than a count.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What the gate found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Findings {
    /// A workspace package that reaches two versions of one external crate.
    pub duplicates: Vec<Duplicate>,
    /// A package that appears in a binary it may not be part of.
    pub intrusions: Vec<String>,
}

/// One workspace package carrying two versions of one crate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Duplicate {
    pub package: String,
    pub crate_name: String,
    /// Every version of it the package reaches, in order.
    pub versions: Vec<String>,
}

impl std::fmt::Display for Duplicate {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            out,
            "`{}` reaches two versions of `{}` ({}); one requirement is looser than it needs to be",
            self.package,
            self.crate_name,
            self.versions.join(" and ")
        )
    }
}

/// The binaries a release ships, and what may not be inside them.
///
/// Derived from the product's own design rather than from a size measurement:
/// these are the packages whose presence in an installer changes what a user's
/// machine has to trust and how large it is, independent of how big any one
/// dependency is.
pub const SHIPPED_BINARIES: &[&str] = &["zup-installer", "zup-dispatch"];

/// Development and distribution tooling, which belongs to the person running a
/// build and to nobody who installs an application.
pub const BUILD_ONLY_PACKAGES: &[&str] = &[
    "zup",
    "zup-build",
    "zup-manifest",
    "zup-plugin-build",
    "zup-publish",
    "zup-publish-github",
    "zup-distribute-github",
    "zup-xtask",
    // The developer CLI's machine contract. It reaches the CLI and the repository's own
    // tooling, and it must never reach an installer: the wire DTOs are a description of
    // a developer's build, and a user installing an application has no build to
    // describe.
    "zup-automation",
];

/// Check the workspace's graphs.
pub fn check(root: &Path) -> Result<Findings, String> {
    let duplicates = workspace_duplicates(root)?;
    let intrusions = intrusions(root)?;
    Ok(Findings {
        duplicates,
        intrusions,
    })
}

impl Findings {
    /// Whether the gate is clean.
    pub fn is_clean(&self) -> bool {
        self.duplicates.is_empty() && self.intrusions.is_empty()
    }
}

/// Every workspace package that reaches two versions of one external crate.
///
/// Read from `cargo metadata`, which resolves the *actual* graph rather than the
/// declared requirements, so a duplicate caused by a feature flag or a target
/// condition is found and a duplicate that only exists in the manifest is not
/// reported.
fn workspace_duplicates(root: &Path) -> Result<Vec<Duplicate>, String> {
    let metadata = cargo_metadata(root)?;
    let workspace: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let mut out: Vec<Duplicate> = Vec::new();
    for node in &metadata.nodes {
        if !workspace.contains(node.name.as_str()) {
            continue;
        }
        // One node per (package, version), so two dependencies with one name and
        // different versions is exactly the condition being tested. A workspace
        // crate is skipped: two of them would be a workspace layout problem, and
        // the package matrices already classify which crate may reach which.
        let mut by_name: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for dependency in &node.dependencies {
            if is_workspace_member(&metadata, &dependency.name) {
                continue;
            }
            by_name
                .entry(dependency.name.as_str())
                .or_default()
                .insert(dependency.version.as_str());
        }
        for (name, versions) in by_name {
            if versions.len() > 1 {
                out.push(Duplicate {
                    package: node.name.clone(),
                    crate_name: name.to_owned(),
                    versions: versions.into_iter().map(str::to_owned).collect(),
                });
            }
        }
    }
    out.sort();
    Ok(out)
}

fn is_workspace_member(metadata: &Metadata, name: &str) -> bool {
    metadata.packages.iter().any(|package| package == name)
}

/// Development tooling that reached a shipped installer.
///
/// The check is over the *resolved* graph, so it fails whether the path is direct
/// (`zup-installer` depending on `zup-publish-github`) or indirect (through a
/// shared crate), which is the case a reviewer's eye misses.
fn intrusions(root: &Path) -> Result<Vec<String>, String> {
    let metadata = cargo_metadata(root)?;
    let shipped: BTreeSet<&str> = SHIPPED_BINARIES.iter().copied().collect();
    let build_only: BTreeSet<&str> = BUILD_ONLY_PACKAGES.iter().copied().collect();

    // One entry per crate name, because the question is which crates a shipped
    // binary can reach, and a crate is reached or it is not regardless of which
    // version of it a path went through.
    let edges: BTreeMap<&str, BTreeSet<&str>> = metadata
        .nodes
        .iter()
        .map(|node| {
            (
                node.name.as_str(),
                node.dependencies
                    .iter()
                    .map(|dependency| dependency.name.as_str())
                    .collect(),
            )
        })
        .collect();

    let mut out = Vec::new();
    for start in &shipped {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        // The path, not just the fact. "zup-publish-github is in the installer
        // graph" is a symptom; "it gets there through zup-distribute-github" is
        // the edge somebody has to delete.
        let mut queue: Vec<Vec<&str>> = vec![vec![*start]];
        while let Some(path) = queue.pop() {
            let name = *path.last().expect("a path is never empty");
            if !seen.insert(name) {
                continue;
            }
            let Some(children) = edges.get(name) else {
                continue;
            };
            for child in children {
                if build_only.contains(child) {
                    out.push(format!(
                        "{}: `{}` is development tooling and reached the `{start}` graph, \
                         which ships to users",
                        path.join(" -> "),
                        child
                    ));
                    continue;
                }
                let mut next = path.clone();
                next.push(child);
                queue.push(next);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `cargo metadata`, as JSON this crate can read without a serialization
/// dependency of its own.
///
/// `cargo_metadata` is a build-time concern of the xtask binary, which already
/// depends on `serde_json`; parsing it here rather than adding a crate keeps the
/// gate's dependency surface at zero.
fn cargo_metadata(root: &Path) -> Result<Metadata, String> {
    let output =
        std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .current_dir(root)
            .args([
                "metadata",
                "--format-version",
                "1",
                "--all-features",
                "--locked",
            ])
            .output()
            .map_err(|error| format!("run `cargo metadata`: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`cargo metadata` failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("`cargo metadata`: {error}"))?;

    let mut packages: BTreeMap<String, String> = BTreeMap::new();
    for package in value["packages"].as_array().cloned().unwrap_or_default() {
        let Some(id) = package["id"].as_str() else {
            continue;
        };
        packages.insert(
            id.to_owned(),
            package["name"].as_str().unwrap_or_default().to_owned(),
        );
    }

    let mut nodes = Vec::new();
    for node in value["resolve"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let Some(id) = node["id"].as_str() else {
            continue;
        };
        let dependencies = node["deps"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|dependency| {
                let pkg = dependency["pkg"].as_str()?;
                if is_dev_only(&dependency["dep_kinds"]) {
                    // `cargo metadata` has no `--no-dev-deps`, so the resolve
                    // graph always contains every package's *test* dependencies.
                    // Walking those would report every workspace package as being
                    // inside every other one, because they all depend on each
                    // other for tests. A test-only edge is not an edge a shipped
                    // binary has.
                    return None;
                }
                // `name` is the *lib* target, which is `atspi-common` for the
                // `atspi_common` package, so the package's own name is read from
                // `pkg`. A build-only crate reached under a renamed lib would
                // otherwise be invisible to the intrusion check.
                Some(Dependency {
                    name: packages.get(pkg).cloned().unwrap_or_else(|| {
                        dependency["name"].as_str().unwrap_or_default().to_owned()
                    }),
                    version: version_of(pkg).to_owned(),
                })
            })
            .collect();
        nodes.push(Node {
            name: packages
                .get(id)
                .cloned()
                .unwrap_or_else(|| name_of(id).to_owned()),
            dependencies,
        });
    }
    Ok(Metadata {
        packages: packages.into_values().collect(),
        nodes,
        workspace_members: value["workspace_members"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect(),
    })
}

/// Whether a dependency edge exists only for a package's own tests.
///
/// `cargo metadata` spells this three ways depending on how many kinds the edge
/// has: an empty string for a normal dependency, a bare `"dev"` string for one
/// dev kind, and an array of `{kind, target}` otherwise. An edge that is
/// *both* normal and dev — a package that uses a crate in production and
/// exercises it in its tests — is not dev-only, and skipping it would hide a real
/// dependency.
fn is_dev_only(dep_kinds: &serde_json::Value) -> bool {
    let kinds: Vec<&str> = match dep_kinds {
        serde_json::Value::String(kind) => vec![kind.as_str()],
        serde_json::Value::Array(entries) => entries
            .iter()
            .map(|entry| entry["kind"].as_str().unwrap_or("normal"))
            .collect(),
        // Absent means cargo did not say, and a gate that skips what it cannot
        // read is a gate with a hole in it.
        _ => return false,
    };
    !kinds.is_empty() && kinds.iter().all(|kind| *kind == "dev")
}

/// The package name inside a resolved node id.
///
/// Cargo emits three shapes, and reading the wrong one is how a gate reports on
/// `0.0.1` instead of on a package:
///
/// - `registry+https://…/index#sha2@0.11.0`
/// - `path+file:///…/crates/zup-core#0.0.1` — the name is the *last path
///   segment*, and only the version sits after the `#`
/// - `some-name 1.2.3 (path+…)` — older cargo
fn name_of(id: &str) -> &str {
    // `registry+…#name@version`: the name is inside the fragment, before the `@`.
    if let Some((_, fragment)) = id.rsplit_once('#')
        && let Some((name, _)) = fragment.split_once('@')
    {
        return name;
    }
    // `path+file:///…/name#version`: the name is the last segment of the path and
    // only the version sits in the fragment.
    if id.starts_with("path+") {
        let path = id.split('#').next().unwrap_or(id);
        return path.rsplit(['/', '\\']).next().unwrap_or(path).trim();
    }
    // `name version (path+…)`, older cargo.
    id.split(' ').next().unwrap_or(id)
}

/// The version inside a resolved node id.
fn version_of(id: &str) -> &str {
    // Both the registry and the path form put the version in the fragment; the
    // space-separated form is the only one that does not.
    if let Some((_, fragment)) = id.rsplit_once('#') {
        return match fragment.split_once('@') {
            Some((_, version)) => version,
            None => fragment,
        };
    }
    id.split(' ').nth(1).unwrap_or("").trim()
}

struct Metadata {
    /// Every package name in the graph. The workspace members are a subset.
    packages: Vec<String>,
    nodes: Vec<Node>,
    workspace_members: BTreeSet<String>,
}

struct Node {
    /// The package name, read out of the resolved id rather than out of `deps`,
    /// whose `name` is the lib target.
    name: String,
    dependencies: Vec<Dependency>,
}

struct Dependency {
    name: String,
    version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_names_a_duplicate_rather_than_counting_one() {
        let duplicate = Duplicate {
            package: "zup-installer".to_owned(),
            crate_name: "sha2".to_owned(),
            versions: vec!["0.10.9".to_owned(), "0.11.0".to_owned()],
        };
        let message = duplicate.to_string();
        assert!(message.contains("zup-installer"), "{message}");
        assert!(message.contains("sha2"), "{message}");
        assert!(message.contains("0.10.9 and 0.11.0"), "{message}");
    }

    #[test]
    fn the_shipped_binaries_and_the_build_tools_are_disjoint() {
        // A package in both lists would make the intrusion check refuse its own
        // starting point, which is a configuration that cannot be right.
        for shipped in SHIPPED_BINARIES {
            assert!(
                !BUILD_ONLY_PACKAGES.contains(shipped),
                "{shipped} is both shipped and build-only"
            );
        }
    }

    /// The three ways cargo spells a dependency kind, and one that means "I could
    /// not tell". Getting the last one wrong in the permissive direction is how a
    /// gate ends up reporting that every workspace package is inside every other
    /// one, because they all depend on each other for tests.
    #[test]
    fn only_an_exclusively_dev_edge_is_skipped() {
        let normal = serde_json::json!({ "kind": null, "target": null });
        assert!(!is_dev_only(&normal), "a normal edge is a real edge");
        assert!(
            !is_dev_only(&serde_json::json!([])),
            "no kinds means cargo said nothing"
        );
        assert!(!is_dev_only(&serde_json::Value::Null));
        assert!(is_dev_only(&serde_json::json!("dev")));
        assert!(is_dev_only(&serde_json::json!([{ "kind": "dev" }])));
        assert!(
            !is_dev_only(&serde_json::json!([{ "kind": "dev" }, { "kind": null }])),
            "a crate that is both a dependency and a dev-dependency is a dependency"
        );
        assert!(!is_dev_only(&serde_json::json!("build")));
    }

    /// A path dependency's name is the last segment of the path, not the version
    /// that sits after the `#`. Reading it wrong collapses every package in the
    /// workspace into one node called `0.0.1`.
    #[test]
    fn a_resolved_id_names_its_package() {
        assert_eq!(
            name_of("registry+https://github.com/rust-lang/crates.io-index#sha2@0.11.0"),
            "sha2"
        );
        assert_eq!(
            name_of("path+file:///C:/work/crates/zup-publish-github#0.0.1"),
            "zup-publish-github"
        );
        assert_eq!(name_of("zup-core 0.0.1 (path+file:///x)"), "zup-core");
        assert_eq!(
            version_of("path+file:///C:/x/crates/zup-core#0.0.1"),
            "0.0.1"
        );
        assert_eq!(
            version_of("registry+https://github.com/rust-lang/crates.io-index#sha2@0.11.0"),
            "0.11.0"
        );
    }
}
