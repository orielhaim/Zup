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
//! - **An internal crate reachable from a published one.** `zup-preset-protocol` and
//!   `zup-preset-sdk` are consumed by preset projects that have never heard of this
//!   repository, so an internal crate in either graph is a type that a preset
//!   author cannot name. The fix is always a conversion at the host boundary, never
//!   a dependency.
//!
//! All three are checked by reading Cargo metadata, so they run offline and take
//! milliseconds, and all three fail with the offending edge rather than a count.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What the gate found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Findings {
    /// A workspace package that reaches two versions of one external crate.
    pub duplicates: Vec<Duplicate>,
    /// A package that appears in a binary it may not be part of.
    pub intrusions: Vec<String>,
    /// An internal crate reachable from a published one.
    pub crossings: Vec<String>,
    /// A crate reaching something its role forbids it to reach.
    pub isolations: Vec<String>,
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
    // The preview environments. They belong to the person writing a preset and
    // the person writing an application, and to nobody installing one, so they
    // must never reach an installer through a shared dependency. `zup-preview`
    // is the machine both of them share; `zup-preset-dev` is the Cargo half of one
    // of them; `zup-preset-compose` is the build-plane answer both must not invent.
    "zup-preview",
    "zup-preset-dev",
    "zup-preset-compose",
    // The developer CLI's machine contract. It reaches the CLI and the repository's own
    // tooling, and it must never reach an installer: the wire DTOs are a description of
    // a developer's build, and a user installing an application has no build to
    // describe.
    "zup-automation",
    // Icon compilation. It runs while a project is built and must not be linked
    // into an installer: the generated files travel with the plan, and the
    // rasterizer does not.
    "zup-assets",
];

/// The crates published for use outside this repository.
///
/// One author-facing name, and two roles behind it that share almost nothing:
///
/// ```text
/// zup-sdk                    the facade, one feature per role
///   preset
///     zup-preset-sdk         the preset authoring API
///       zup-preset-sdk-macros
///       zup-preset-protocol  the wire, read by the SDK and the transport
///       zup-preset-ipc       the transport that carries it
///   plugin
///     zup-plugin-sdk         the plugin authoring API and its bindings
///       zup-plugin-abi       the Component Model contract, the canonical ABI
/// ```
///
/// An author names `zup-sdk` and one role. Everything below it is published
/// because Cargo resolves a transitive dependency from crates.io, not because
/// anyone should reach it directly: a published crate that names `zup-windows` is
/// a crate that can only be built inside the repository that owns it, which is
/// the opposite of what publishing one is for.
///
/// The gate treats all seven as roots, so a new edge from any of them into the
/// engine is caught whether it is written in the facade, in a role's API, in a
/// transport, or in a contract.
pub const PUBLISHED_PACKAGES: &[&str] = &[
    "zup-sdk",
    "zup-preset-sdk",
    "zup-preset-sdk-macros",
    "zup-preset-protocol",
    "zup-preset-ipc",
    "zup-plugin-sdk",
    "zup-plugin-abi",
];

/// What a crate may not reach, because reaching it would defeat a boundary.
///
/// Each entry is a rule about one thing rather than a list of crates to check
/// for, so a new crate is caught by the property it violates rather than by
/// having to be remembered here. These are the properties the architecture is
/// made of, and a graph that satisfies them is the graph the design claims.
pub const ISOLATION: &[(&str, &[&str], &str)] = &[
    (
        // A plugin guest is WebAssembly with no host behind it. A runtime in its
        // graph is a runtime the guest carries into every application it ships
        // inside, and an engine API is an engine API whether or not the plugin
        // can call it from inside a sandbox.
        "zup-plugin-sdk",
        &[
            "wasmtime",
            "zup-plugin-contract",
            "zup-plugin-runtime",
            "zup-plugin-build",
        ],
        "a plugin guest must not carry the runtime that executes it",
    ),
    (
        // The same reasoning for the contract: it is data both sides read, and a
        // guest that pulls it also pulls the engine.
        "zup-plugin-abi",
        &["wasmtime", "zup-plugin-contract", "zup-plugin-runtime"],
        "the plugin contract is data, and must not carry the engine that reads it",
    ),
    (
        // A preset is a window. It reaches the GPUI stack and nothing that knows
        // what an installation is doing underneath it.
        "zup-preset-sdk",
        &[
            "wasmtime",
            "zup-preset-host",
            "zup-preset-compose",
            "zup-preset-dev",
            "zup-installer",
            "zup-preview",
            "zup-runtime",
            "zup-plan",
            "zup-transaction",
            "zup-exec",
            "zup-windows",
        ],
        "a preset draws what the host publishes and reaches nothing beneath it",
    ),
    (
        // The wire contract is pure data by construction, and that is what makes
        // it publishable at all: anything platform-shaped in it would be a crate
        // a preset on another platform cannot compile.
        "zup-preset-protocol",
        &[
            "gpui-kit",
            "wasmtime",
            "tokio",
            "zup-core",
            "zup-preset-ipc",
        ],
        "the preset contract is pure data, and carries neither the window nor the engine",
    ),
    (
        // The transport is the operating system's own IPC and the frames above
        // it. It reaches no engine, because a preset that reaches an engine has a
        // plan.
        "zup-preset-ipc",
        &["wasmtime", "zup-core", "zup-preset-host"],
        "the preset transport moves frames and reaches nothing that decides anything",
    ),
];

/// Check the workspace's graphs.
pub fn check(root: &Path) -> Result<Findings, String> {
    let metadata = cargo_metadata(root)?;
    let edges = edges(&metadata);
    Ok(Findings {
        duplicates: workspace_duplicates(&metadata),
        intrusions: intrusions(&edges),
        crossings: crossings(&metadata, &edges),
        isolations: isolations(&metadata, &edges),
    })
}

impl Findings {
    /// Whether the gate is clean.
    pub fn is_clean(&self) -> bool {
        self.duplicates.is_empty()
            && self.intrusions.is_empty()
            && self.crossings.is_empty()
            && self.isolations.is_empty()
    }
}

/// Every workspace package that reaches two versions of one external crate.
///
/// Read from `cargo metadata`, which resolves the *actual* graph rather than the
/// declared requirements, so a duplicate caused by a feature flag or a target
/// condition is found and a duplicate that only exists in the manifest is not
/// reported.
fn workspace_duplicates(metadata: &Metadata) -> Vec<Duplicate> {
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
            if is_workspace_member(metadata, &dependency.name) {
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
    out
}

fn is_workspace_member(metadata: &Metadata, name: &str) -> bool {
    metadata.workspace_members.contains(name)
}

/// The resolved graph as one adjacency list per crate name.
type Edges<'a> = BTreeMap<&'a str, BTreeSet<&'a str>>;

fn edges(metadata: &Metadata) -> Edges<'_> {
    metadata
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
        .collect()
}

/// Every path from `roots` to a crate named in `wanted`, as `a -> b -> c`.
///
/// The walk stops at the first crate it is looking for, because the edge that put
/// that crate in the graph is the one somebody has to delete; whatever sits beyond
/// it is only reachable through the same mistake. Cycles are bounded by what has
/// already been seen.
fn paths_to<'a>(
    edges: &Edges<'a>,
    roots: &[&'a str],
    wanted: &BTreeSet<&'a str>,
) -> Vec<Vec<&'a str>> {
    let mut out: Vec<Vec<&'a str>> = Vec::new();
    for start in roots {
        let mut seen: BTreeSet<&'a str> = BTreeSet::new();
        let mut queue: Vec<Vec<&'a str>> = vec![vec![*start]];
        while let Some(path) = queue.pop() {
            let name = *path.last().expect("a path is never empty");
            if !seen.insert(name) {
                continue;
            }
            if path.len() > 1 && wanted.contains(name) {
                out.push(path);
                continue;
            }
            let Some(children) = edges.get(name) else {
                continue;
            };
            for child in children {
                if seen.contains(child) {
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
    out
}

/// Development tooling that reached a shipped installer.
///
/// The check is over the *resolved* graph, so it fails whether the path is direct
/// (`zup-installer` depending on `zup-publish-github`) or indirect (through a
/// shared crate), which is the case a reviewer's eye misses.
fn intrusions(edges: &Edges<'_>) -> Vec<String> {
    let build_only: BTreeSet<&str> = BUILD_ONLY_PACKAGES.iter().copied().collect();
    // One entry per crate name, because the question is which crates a shipped
    // binary can reach, and a crate is reached or it is not regardless of which
    // version of it a path went through.
    let mut out: Vec<String> = paths_to(edges, SHIPPED_BINARIES, &build_only)
        .into_iter()
        .map(|path| {
            format!(
                "{}: `{}` is development tooling and reached the `{}` graph, which ships to users",
                path.join(" -> "),
                path[path.len() - 1],
                path[0]
            )
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// An internal crate a published crate now reaches.
///
/// One report per offending edge rather than per path, because the edge is what
/// somebody has to delete, and two public crates reaching the same internal one is
/// one mistake rather than two.
fn crossings(metadata: &Metadata, edges: &Edges<'_>) -> Vec<String> {
    let public: BTreeSet<&str> = PUBLISHED_PACKAGES.iter().copied().collect();
    let roots: Vec<&str> = PUBLISHED_PACKAGES
        .iter()
        .copied()
        .filter(|root| is_workspace_member(metadata, root))
        .collect();
    let internal: BTreeSet<&str> = metadata
        .nodes
        .iter()
        .map(|node| node.name.as_str())
        .filter(|name| is_workspace_member(metadata, name) && !public.contains(name))
        .collect();
    let mut reported: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for path in paths_to(edges, &roots, &internal) {
        let edge = (path[path.len() - 2], path[path.len() - 1]);
        if !reported.insert(edge) {
            continue;
        }
        out.push(format!(
            "{}: `{}` is an internal zup crate and a published crate may not reach it; \
             convert at the host boundary instead",
            path.join(" -> "),
            path[path.len() - 1]
        ));
    }
    out
}

/// A crate reaching something its role forbids.
///
/// One report per offending edge, and the path that got there, because the edge
/// is what somebody has to delete and the path is what tells them which of two
/// plausible edges actually did it. An isolation rule is skipped for a crate no
/// matrix claims, rather than reported: the rule describes a role, and a crate
/// with no role has nothing to violate.
fn isolations(metadata: &Metadata, edges: &Edges<'_>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (crate_name, forbidden, reason) in ISOLATION {
        if !is_workspace_member(metadata, crate_name) {
            continue;
        }
        let wanted: BTreeSet<&str> = forbidden.iter().copied().collect();
        for path in paths_to(edges, &[*crate_name], &wanted) {
            out.push(format!(
                "{}: {} (`{}`)",
                path.join(" -> "),
                reason,
                path[path.len() - 1]
            ));
        }
    }
    out.sort();
    out.dedup();
    out
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
    // Cargo names a workspace member by its resolved id, which is not its name.
    // Comparing a node's name against these ids would never match, and every check
    // that asks "is this one of ours?" would quietly pass on everything.
    let workspace_members: BTreeSet<String> = value["workspace_members"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|id| id.as_str())
        .map(|id| {
            packages
                .get(id)
                .cloned()
                .unwrap_or_else(|| name_of(id).to_owned())
        })
        .collect();

    Ok(Metadata {
        nodes,
        workspace_members,
    })
}

/// Whether a dependency edge exists only for a package's own tests.
///
/// `cargo metadata` spells this three ways depending on how many kinds the edge
/// has: an empty string for a normal dependency, a bare `"dev"` string for one
/// dev kind, and an array of `{kind, target}` otherwise. An edge that is
/// *both* normal and dev - a package that uses a crate in production and
/// exercises it in its tests - is not dev-only, and skipping it would hide a real
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
/// - `path+file:///…/crates/zup-core#0.0.1` - the name is the *last path
///   segment*, and only the version sits after the `#`
/// - `some-name 1.2.3 (path+…)` - older cargo
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
    nodes: Vec<Node>,
    /// The workspace's own packages, by name.
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

    /// A package in both lists would make the intrusion check refuse its own
    /// starting point, which is a configuration that cannot be right.
    #[test]
    fn the_shipped_binaries_and_the_build_tools_are_disjoint() {
        for shipped in SHIPPED_BINARIES {
            assert!(
                !BUILD_ONLY_PACKAGES.contains(shipped),
                "{shipped} is both shipped and build-only"
            );
        }
    }

    /// A published crate that reaches an internal one fails; one that reaches only
    /// another published crate passes. The SDK may depend on the protocol, because
    /// that dependency is what a preset project resolves from crates.io.
    #[test]
    fn a_published_crate_may_not_reach_an_internal_one() {
        let members: BTreeSet<String> = [
            "zup-core",
            "zup-installer",
            "zup-runtime",
            "zup-preset-protocol",
            "zup-preset-sdk",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let clean = Metadata {
            nodes: vec![
                node("zup-preset-protocol", &[]),
                node("zup-preset-sdk", &["zup-preset-protocol"]),
                node("zup-installer", &["zup-core", "zup-runtime"]),
                node("zup-core", &[]),
                node("zup-runtime", &["zup-core"]),
            ],
            workspace_members: members.clone(),
        };
        assert!(crossings(&clean, &edges(&clean)).is_empty());

        // The same graph with one edge added. It is reached through the protocol
        // rather than declared directly, which is the case a reviewer's eye misses
        // because `zup-preset-sdk -> zup-preset-protocol` is a dependency that is supposed
        // to be there, and it is reported once, at the edge that is not.
        let leaked = Metadata {
            nodes: vec![
                node("zup-preset-protocol", &["zup-core"]),
                node("zup-preset-sdk", &["zup-preset-protocol"]),
                node("zup-installer", &["zup-core"]),
                node("zup-core", &[]),
                node("zup-runtime", &["zup-core"]),
            ],
            workspace_members: members,
        };
        assert_eq!(
            crossings(&leaked, &edges(&leaked)),
            [
                "zup-preset-protocol -> zup-core: `zup-core` is an internal zup crate \
             and a published crate may not reach it; convert at the host boundary instead"
            ]
        );
    }

    /// A duplicate is only reported for a workspace member, which means the
    /// membership test has to agree with cargo about which packages those are.
    /// Reading a resolved id as a name here made the whole check vacuous.
    #[test]
    fn a_duplicate_is_found_for_a_workspace_member() {
        let metadata = Metadata {
            nodes: vec![Node {
                name: "zup-core".to_owned(),
                dependencies: vec![
                    dependency("serde", "1.0.219"),
                    dependency("serde", "1.0.228"),
                ],
            }],
            workspace_members: BTreeSet::from(["zup-core".to_owned()]),
        };
        assert_eq!(
            workspace_duplicates(&metadata),
            [Duplicate {
                package: "zup-core".to_owned(),
                crate_name: "serde".to_owned(),
                versions: vec!["1.0.219".to_owned(), "1.0.228".to_owned()],
            }]
        );
    }

    fn node(name: &str, dependencies: &[&str]) -> Node {
        Node {
            name: name.to_owned(),
            dependencies: dependencies
                .iter()
                .map(|name| dependency(name, "0.0.1"))
                .collect(),
        }
    }

    fn dependency(name: &str, version: &str) -> Dependency {
        Dependency {
            name: name.to_owned(),
            version: version.to_owned(),
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
