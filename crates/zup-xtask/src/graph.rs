use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::matrix;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Findings {
    pub duplicates: Vec<Duplicate>,
    pub intrusions: Vec<String>,
    pub crossings: Vec<String>,
    pub isolations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Duplicate {
    pub package: String,
    pub crate_name: String,
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

pub const SHIPPED_BINARIES: &[&str] = &["zup-installer", "zup-dispatch"];

pub const BUILD_ONLY_PACKAGES: &[&str] = &[
    "zup",
    "zup-build",
    "zup-manifest",
    "zup-plugin-build",
    "zup-publish",
    "zup-publish-github",
    "zup-distribute-github",
    "zup-xtask",
    "zup-preview",
    "zup-preset-dev",
    "zup-preset-compose",
    "zup-automation",
    "zup-assets",
];

pub const PUBLISHED_PACKAGES: &[&str] = &[
    "zup-sdk",
    "zup-sdk-macros",
    "zup-preset-protocol",
    "zup-preset-ipc",
    "zup-plugin-abi",
];

pub const ISOLATION: &[(&str, &[&str], &str)] = &[
    (
        "zup-sdk",
        &[
            "wasmtime",
            "zup-plugin-contract",
            "zup-plugin-runtime",
            "zup-plugin-build",
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
        "an authoring role carries its own machinery and reaches nothing beneath the host boundary",
    ),
    (
        "zup-plugin-abi",
        &["wasmtime", "zup-plugin-contract", "zup-plugin-runtime"],
        "the plugin contract is data, and must not carry the engine that reads it",
    ),
    (
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
        "zup-preset-ipc",
        &["wasmtime", "zup-core", "zup-preset-host"],
        "the preset transport moves frames and reaches nothing that decides anything",
    ),
];

pub fn check(root: &Path) -> Result<Findings, String> {
    let metadata = cargo_metadata(root)?;
    let edges = edges(&metadata);
    Ok(Findings {
        duplicates: workspace_duplicates(&metadata),
        intrusions: intrusions(&edges),
        crossings: crossings(&metadata, &edges),
        isolations: isolations(&metadata, &edges)
            .into_iter()
            .chain(backend_crossings(&metadata, &edges))
            .collect(),
    })
}

impl Findings {
    pub fn is_clean(&self) -> bool {
        self.duplicates.is_empty()
            && self.intrusions.is_empty()
            && self.crossings.is_empty()
            && self.isolations.is_empty()
    }
}

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

fn intrusions(edges: &Edges<'_>) -> Vec<String> {
    let build_only: BTreeSet<&str> = BUILD_ONLY_PACKAGES.iter().copied().collect();
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

fn backend_crossings(metadata: &Metadata, edges: &Edges<'_>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for platform in matrix::Platform::ALL {
        let crate_name = platform.crate_name();
        if !is_workspace_member(metadata, crate_name) {
            continue;
        }
        let siblings: BTreeSet<&str> = matrix::sibling_backends(*platform)
            .into_iter()
            .filter(|sibling| is_workspace_member(metadata, sibling))
            .collect();
        for path in paths_to(edges, &[crate_name], &siblings) {
            out.push(format!(
                "{}: `{}` is a native backend and may not reach `{}`, which is another platform's \
                 mechanism layer",
                path.join(" -> "),
                crate_name,
                path[path.len() - 1]
            ));
        }
    }
    out.sort();
    out.dedup();
    out
}

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
                    return None;
                }
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

fn is_dev_only(dep_kinds: &serde_json::Value) -> bool {
    let kinds: Vec<&str> = match dep_kinds {
        serde_json::Value::String(kind) => vec![kind.as_str()],
        serde_json::Value::Array(entries) => entries
            .iter()
            .map(|entry| entry["kind"].as_str().unwrap_or("normal"))
            .collect(),
        _ => return false,
    };
    !kinds.is_empty() && kinds.iter().all(|kind| *kind == "dev")
}

fn name_of(id: &str) -> &str {
    if let Some((_, fragment)) = id.rsplit_once('#')
        && let Some((name, _)) = fragment.split_once('@')
    {
        return name;
    }
    if id.starts_with("path+") {
        let path = id.split('#').next().unwrap_or(id);
        return path.rsplit(['/', '\\']).next().unwrap_or(path).trim();
    }
    id.split(' ').next().unwrap_or(id)
}

fn version_of(id: &str) -> &str {
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
    workspace_members: BTreeSet<String>,
}

struct Node {
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
    fn the_shipped_binaries_and_the_build_tools_are_disjoint() {
        for shipped in SHIPPED_BINARIES {
            assert!(
                !BUILD_ONLY_PACKAGES.contains(shipped),
                "{shipped} is both shipped and build-only"
            );
        }
    }

    #[test]
    fn a_published_crate_may_not_reach_an_internal_one() {
        let members: BTreeSet<String> = [
            "zup-core",
            "zup-installer",
            "zup-runtime",
            "zup-preset-protocol",
            "zup-sdk",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let clean = Metadata {
            nodes: vec![
                node("zup-preset-protocol", &[]),
                node("zup-sdk", &["zup-preset-protocol"]),
                node("zup-installer", &["zup-core", "zup-runtime"]),
                node("zup-core", &[]),
                node("zup-runtime", &["zup-core"]),
            ],
            workspace_members: members.clone(),
        };
        assert!(crossings(&clean, &edges(&clean)).is_empty());

        let leaked = Metadata {
            nodes: vec![
                node("zup-preset-protocol", &["zup-core"]),
                node("zup-sdk", &["zup-preset-protocol"]),
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

    #[test]
    fn a_backend_may_not_reach_its_sibling_in_either_direction() {
        let members: BTreeSet<String> = matrix::backends()
            .iter()
            .map(|backend| (*backend).to_owned())
            .collect();
        let windows = "zup-windows";
        let linux = "zup-linux";

        let clean = Metadata {
            nodes: vec![
                node(windows, &["zup-core"]),
                node(linux, &["zup-core"]),
                node("zup-core", &[]),
            ],
            workspace_members: members.clone(),
        };
        assert_eq!(
            backend_crossings(&clean, &edges(&clean)),
            Vec::<String>::new()
        );

        let leaked = Metadata {
            nodes: vec![
                node(windows, &["zup-linux"]),
                node(linux, &["zup-core"]),
                node("zup-core", &[]),
            ],
            workspace_members: members.clone(),
        };
        let reported = backend_crossings(&leaked, &edges(&leaked));
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert!(
            reported[0].contains("zup-windows -> zup-linux"),
            "{reported:?}"
        );

        let indirect = Metadata {
            nodes: vec![
                node(linux, &["zup-core"]),
                node(windows, &["zup-core"]),
                node("zup-core", &["zup-linux"]),
            ],
            workspace_members: members,
        };
        let reported = backend_crossings(&indirect, &edges(&indirect));
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert!(
            reported[0].contains("zup-windows -> zup-core -> zup-linux"),
            "{reported:?}"
        );
    }

    #[test]
    fn a_backend_absent_from_the_workspace_is_skipped() {
        let metadata = Metadata {
            nodes: vec![node("zup-windows", &["zup-core"]), node("zup-core", &[])],
            workspace_members: BTreeSet::from(["zup-windows".to_owned(), "zup-core".to_owned()]),
        };
        assert_eq!(
            backend_crossings(&metadata, &edges(&metadata)),
            Vec::<String>::new()
        );
    }

    fn dependency(name: &str, version: &str) -> Dependency {
        Dependency {
            name: name.to_owned(),
            version: version.to_owned(),
        }
    }

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
