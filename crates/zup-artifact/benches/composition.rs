//! What composition costs and saves, measured on a realistic payload.
//!
//! Every number a decision needs in one place: what a set of variants costs
//! shipped separately, what one composed artifact costs, what the store holds,
//! what a machine keeps after installing one variant, and what each compression
//! level buys. The fixture is megabyte-scale because a kilobyte-scale fixture is
//! dominated by the launcher and would answer nothing.
//!
//! Run with `cargo bench -p zup-artifact --bench composition`.

use std::collections::BTreeMap;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use sha2::{Digest, Sha256};
use zup_artifact::{
    ArtifactComposer, ArtifactGraph, ArtifactRequest, DistributionVariant, MediaType,
};
use zup_core::{
    App, AppId, Component, FileMapping, Frontend, Install, InstallDirectory, InstallScope,
    NonEmptyString, RelativePath, ResolvedTargetConfig, TargetProfileId, TargetTriple, Template,
    UpdateConfig,
};
use zup_plugin_contract::{AOT_FORMAT_VERSION, PLUGIN_API_VERSION, WASMTIME_VERSION};

/// The three machines a Windows application is usually built for.
const TARGETS: &[(&str, &str, u64)] = &[
    ("windows-x86", "i686-pc-windows-msvc", 1),
    ("windows-x64", "x86_64-pc-windows-msvc", 2),
    ("windows-arm64", "aarch64-pc-windows-msvc", 3),
];

/// A managed runtime, an asset bundle, and a licence, in mebibytes. This is the
/// content that is byte-identical on every machine and is most of an
/// application's size.
const SHARED: &[(&str, usize)] = &[
    ("runtime/framework.dat", 220),
    ("assets/bundle.pak", 48),
    ("docs/license.txt", 2),
];

/// Architecture-specific binaries, in mebibytes.
const EXCLUSIVE: &[(&str, usize)] = &[("bin/Acme.exe", 64), ("bin/acme-agent.exe", 18)];

/// One variant's native runtime template, in mebibytes. A real one is far
/// larger; this is the *incremental* cost of adding a machine to an artifact,
/// which is what composition is measured against.
const RUNTIME_MEBIBYTES: usize = 1;

fn app() -> App {
    App {
        id: AppId::new("com.acme.desktop").unwrap(),
        name: NonEmptyString::new("Acme").unwrap(),
        version: semver::Version::parse("1.4.0").unwrap(),
        publisher: Some(NonEmptyString::new("Acme Inc.").unwrap()),
        main: Some(Template::parse("${install}/bin/Acme.exe").unwrap()),
        description: None,
    }
}

fn install() -> Install {
    Install {
        scope: InstallScope::Either,
        directory: InstallDirectory {
            user: Some(Template::parse("${location.programs}/Acme").unwrap()),
            machine: Some(Template::parse("${location.programs}/Acme").unwrap()),
        },
        allow_directory_override: true,
    }
}

fn updates() -> UpdateConfig {
    UpdateConfig {
        repository: "https://releases.acme.test/acme".to_owned(),
        channel: "stable".to_owned(),
        trusted_root: b"zup trusted root bytes".to_vec(),
    }
}

/// Incompressible bytes that are nonetheless exactly reproducible, so a shared
/// file really is shared rather than coincidentally equal.
fn payload(seed: &[u8], mebibytes: usize) -> Vec<u8> {
    let size = mebibytes * 1024 * 1024;
    let mut out = Vec::with_capacity(size + 32);
    let mut block = 0u64;
    while out.len() < size {
        let mut hasher = Sha256::new();
        hasher.update(seed);
        hasher.update(block.to_le_bytes());
        out.extend_from_slice(&hasher.finalize());
        block += 1;
    }
    out.truncate(size);
    out
}

fn build(root: &std::path::Path, profile: &str, target: &str, tag: u64) -> DistributionVariant {
    let source = root.join(profile);
    let resolved = ResolvedTargetConfig {
        profile: TargetProfileId::new(profile).unwrap(),
        target: TargetTriple::parse(target).unwrap(),
        source: zup_core::Source::new(source.clone()).unwrap(),
        frontend: Frontend::Console,
        install: install(),
    };

    let mut files: Vec<(&str, Vec<u8>)> = SHARED
        .iter()
        .map(|(name, size)| (*name, payload(name.as_bytes(), *size)))
        .collect();
    for (name, size) in EXCLUSIVE {
        let mut seed = tag.to_le_bytes().to_vec();
        seed.extend_from_slice(name.as_bytes());
        files.push((name, payload(&seed, *size)));
    }

    let mut resolved_files = Vec::new();
    let mut total = 0u64;
    for (name, content) in &files {
        let path = source.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        let size = content.len() as u64;
        total += size;
        resolved_files.push(zup_build::ResolvedFile {
            source: path,
            source_relative: RelativePath::new(name).unwrap(),
            destination: Template::parse(&format!("${{install}}/{name}")).unwrap(),
            size,
            sha256: zup_core::Sha256Digest::from_bytes(Sha256::digest(content).into()),
            component: None,
            condition: None,
        });
    }
    drop(files);

    let mut installer_files: Vec<FileMapping> = resolved_files
        .iter()
        .map(|file| FileMapping {
            source: "embedded".to_owned(),
            destination: file.destination.clone(),
            component: None,
            when: None,
            allow_empty: false,
        })
        .collect();
    installer_files.sort_by(|left, right| {
        left.destination
            .to_string()
            .cmp(&right.destination.to_string())
    });

    // Each machine gets its own ahead-of-time plugin, which is a real cost of
    // targeting more than one architecture.
    let mut plugin_bytes = payload(b"plugin-aot", RUNTIME_MEBIBYTES);
    plugin_bytes.extend_from_slice(target.as_bytes());
    let plugin_digest = zup_core::Sha256Digest::from_bytes(Sha256::digest(&plugin_bytes).into());
    let plugin = zup_bundle::CompiledPluginArtifact::new(
        zup_bundle::PluginArtifact {
            plugin_id: zup_core::PluginId::new("acme-plugins").unwrap(),
            source_size: 16,
            source_sha256: zup_core::Sha256Digest::from_bytes(Sha256::digest(b"source").into()),
            target: resolved.target.clone(),
            wasmtime_version: WASMTIME_VERSION.to_owned(),
            aot_format_version: AOT_FORMAT_VERSION,
            plugin_api_version: PLUGIN_API_VERSION.to_owned(),
            wit_digest: zup_core::Sha256Digest::from_bytes(
                zup_plugin_contract::wit_package_digest(),
            ),
            engine_fingerprint: zup_core::Sha256Digest::from_bytes(
                *zup_plugin_contract::engine_fingerprint(target).as_bytes(),
            ),
            aot_size: plugin_bytes.len() as u64,
            aot_sha256: plugin_digest,
            blob: plugin_digest,
        },
        plugin_bytes,
    )
    .unwrap();

    let mut runtime = payload(b"runtime-image", RUNTIME_MEBIBYTES);
    runtime.extend_from_slice(target.as_bytes());

    DistributionVariant::resolve(
        &resolved,
        &zup_build::TargetBuildPlan {
            installer: zup_core::Installer {
                app: app(),
                target: resolved.target.clone(),
                frontend: Frontend::Console,
                preset: None,
                updates: Some(updates()),
                install: install(),
                prerequisites: Vec::new(),
                components: vec![Component {
                    id: zup_core::ComponentId::new("core").unwrap(),
                    name: NonEmptyString::new("Application").unwrap(),
                    description: None,
                    required: true,
                    default: true,
                    requires: Vec::new(),
                    group: None,
                }],
                component_groups: Vec::new(),
                plugins: vec![zup_core::PluginBinding {
                    id: plugin.metadata().plugin_id.clone(),
                    component: None,
                    when: None,
                }],
                files: installer_files,
                launchers: Vec::new(),
                path: Vec::new(),
                services: Vec::new(),
                protocols: Vec::new(),
                file_associations: Vec::new(),
            },
            prerequisites: Vec::new(),
            plugins: Vec::new(),
            files: resolved_files,
            ui_assets: Vec::new(),
            total_size: total,
            prerequisite_size: 0,
        },
        &[plugin],
        &[(MediaType::RUNTIME, runtime)],
    )
    .unwrap()
}

/// A fixture built once, shared by every measurement below.
struct Fixture {
    #[allow(dead_code)]
    root: tempfile::TempDir,
    variants: Vec<DistributionVariant>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        let variants = TARGETS
            .iter()
            .map(|(profile, target, tag)| build(root.path(), profile, target, *tag))
            .collect();
        Self { root, variants }
    }

    fn references(&self) -> Vec<&DistributionVariant> {
        self.variants.iter().collect()
    }

    fn compose(&self) -> ArtifactGraph {
        let composed = self.references();
        ArtifactComposer::new(
            ArtifactRequest::universal_offline("windows", &app(), "Acme-Windows-Setup.exe"),
            &composed,
        )
        .expect("composable")
        .compose(&composed)
        .expect("composed")
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn report(fixture: &Fixture, graph: &ArtifactGraph) {
    let savings = graph.savings();
    let table = graph.table();

    let mut references: BTreeMap<zup_core::Sha256Digest, usize> = BTreeMap::new();
    for variant in &fixture.variants {
        for digest in variant.content_digests() {
            *references.entry(digest).or_default() += 1;
        }
    }
    let shared_blobs = references.values().filter(|count| **count > 1).count();

    println!("\n=== Composition, {} targets ===", TARGETS.len());
    println!("\n-- what a user downloads --");
    println!(
        "{:>28} {:>8.1} MiB",
        format!("{} separate installers", TARGETS.len()),
        mib(savings.standalone_size)
    );
    println!(
        "{:>28} {:>8.1} MiB",
        "one universal artifact",
        mib(savings.standalone_size - savings.shared_size)
    );
    println!(
        "{:>28} {:>8.1} MiB",
        "saved by composing",
        mib(savings.shared_size)
    );
    println!(
        "{:>28} {:>7.1}%",
        "reduction",
        savings.shared_size as f64 / savings.standalone_size as f64 * 100.0
    );

    println!("\n-- what the store holds --");
    println!(
        "{:>28} {:>8.1} MiB",
        "stored, compressed",
        mib(table.stored_size())
    );
    println!(
        "{:>28} {:>8.1} MiB",
        "content, uncompressed",
        mib(table.logical_size())
    );
    // The payload is incompressible by construction, so that compression is not
    // a variable here and deduplication is the only thing being measured. The
    // `compress` benchmark below is where compression is measured.
    println!(
        "{:>28} {:>8.1}%",
        "compression",
        table.stored_size() as f64 / table.logical_size() as f64 * 100.0
    );
    println!("{:>28} {:>8}", "unique blobs", savings.unique_blob_count);
    println!("{:>28} {:>8}", "shared blobs", shared_blobs);
    println!(
        "{:>28} {:>8}",
        "shared of all blobs",
        format!(
            "{:.0}%",
            shared_blobs as f64 / references.len() as f64 * 100.0
        )
    );

    // What one machine keeps after installing one variant.
    let id = TARGETS[1].0;
    let one = graph.index().variant(id).expect("the x64 variant");
    let kept: u64 = graph
        .content_of(id)
        .expect("the x64 content set")
        .digests
        .iter()
        .map(|digest| table.entry(digest).map_or(0, |entry| entry.size))
        .sum();
    let kept_runtime = one.runtime.map_or(0, |bytes| bytes.size);
    let not_kept = table.stored_size().saturating_sub(kept);

    println!("\n-- after installing {} on an x64 machine --", one.target);
    println!(
        "{:>28} {:>8.1} MiB",
        "kept: this variant's content",
        mib(kept)
    );
    println!(
        "{:>28} {:>8.1} MiB",
        "kept: this variant's runtime",
        mib(kept_runtime)
    );
    println!(
        "{:>28} {:>8.1} MiB",
        "not kept: other machines'",
        mib(not_kept)
    );
    println!(
        "{:>28} {:>7.1}%",
        "of the artifact retained",
        (kept + kept_runtime) as f64 / table.stored_size() as f64 * 100.0
    );
}

fn timings(fixture: &Fixture, criterion: &mut Criterion) {
    let graph = fixture.compose();

    let mut compose = criterion.benchmark_group("compose");
    for count in 1..=TARGETS.len() {
        let subset: Vec<&DistributionVariant> = fixture.variants.iter().take(count).collect();
        compose.bench_with_input(
            BenchmarkId::from_parameter(count),
            &subset,
            |bencher, subset| {
                bencher.iter_batched(
                    || subset.clone(),
                    |composed| {
                        ArtifactComposer::new(
                            ArtifactRequest::universal_offline(
                                "windows",
                                &app(),
                                "Acme-Windows-Setup.exe",
                            ),
                            &composed,
                        )
                        .unwrap()
                        .compose(&composed)
                        .unwrap()
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }
    compose.finish();

    let mut read = criterion.benchmark_group("encode");
    read.bench_function("the index", |bencher| {
        bencher.iter(|| graph.index_bytes().unwrap());
    });
    read.bench_function("the blob table", |bencher| {
        bencher.iter(|| graph.table_bytes().unwrap());
    });
    read.bench_function("every variant manifest", |bencher| {
        bencher.iter(|| graph.manifest_bytes().unwrap());
    });
    read.bench_function("every native runtime", |bencher| {
        bencher.iter(|| graph.runtime_bytes().unwrap());
    });
    read.finish();
}

fn compression(criterion: &mut Criterion) {
    // The largest thing an application ships, which is what a compression level
    // is chosen for. Incompressible on purpose: a level that cannot shrink this
    // will not shrink anything, and one that can will be paying for it.
    let bytes = payload(b"largest-blob", 64);
    let mut group = criterion.benchmark_group("compress 64 MiB");
    for level in [1, 3, 9, 15, 19] {
        group.bench_with_input(
            BenchmarkId::new("zstd", format!("level {level}")),
            &bytes,
            |bencher, bytes| {
                bencher.iter(|| zstd::stream::encode_all(bytes.as_slice(), level).unwrap())
            },
        );
    }
    group.finish();
}

fn composition(criterion: &mut Criterion) {
    let fixture = Fixture::new();
    report(&fixture, &fixture.compose());
    timings(&fixture, criterion);
    compression(criterion);
}

criterion_group!(benches, composition);
criterion_main!(benches);
