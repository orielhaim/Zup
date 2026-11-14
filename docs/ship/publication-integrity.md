# GitHub publication integrity: F21–F23

The publisher now binds one finite inventory to the staged release, its finalized
installer bytes, and an exact remote Git tag object and peeled commit. The generated
workflow transfers that same release through composition, optional signing,
finalization, optional attestation, and publication.

## Reproductions

Before changing the implementation, three executable regressions passed against
the old behavior (`cargo nextest ... -E 'test(reproduce_f)'`, 3 passed):

- A real transport writer produced multiple shards and its JSON descriptor.
  Both filename-based collection helpers returned empty lists. This initial
  reproduction used one transport variant; the replacement regression uses two.
- The default unsigned workflow omitted the signing job but referenced it from
  downstream jobs. Its plan output also referenced a step without an ID.
- A loopback GitHub server accepted release creation and upload despite a source
  claim that had never been checked against a remote tag.

The replacement tests assert the repaired contracts rather than preserving those
unsafe outcomes.

## Inventory and transport

`ReleaseManifest` records installers, logical transport packages and their pinned
descriptors, web documents, and raw native objects. A multipart logical package is
not uploaded as a nonexistent `.zup` file: its descriptor and every physical shard
are products. An unsharded package is its one physical `.zup` product. Logical
package digests hash the concatenated transport bytes; each shard has its own hash.

Composition imports verified matrix installer bytes. It does not rebuild payloads
from a source directory on the composition runner. Package metadata, embedded
variant manifests, native runtime/preset objects, and payload frames retain their
digest relationships. This path inspects portable containers without executing
native code or deserializing WASI AOT artifacts.

Publication validates application, target, variant, declared origins, document and
catalog closure, descriptor identity, contiguous ordered shards, exact sizes and
hashes, and decompressed payload hashes. It rejects foreign files in transport/web
namespaces, unknown metadata, missing chunks, extra chunks, forged outer checksums,
traversal, and symlink/reparse paths. It never recursively adopts arbitrary source
files. Matrix entries must have a real release manifest.

Each descriptor depends on its physical shards; the release manifest depends on
the remaining products. Plan validation rejects missing dependencies and cycles.
An asset shared by roles counts once toward GitHub limits. Only internal transport
products are shardable. Installer and raw-native download contracts remain intact;
an indivisible frame or metadata section larger than the shard limit is refused.

The adapter snapshots every local product into private temporary storage and
verifies size and digest before any network operation. Uploads read those verified
snapshots. This uses additional temporary disk space approximately equal to the
publication size and requires sequential hashing and transport-validation reads.

Published assets are immutable to this adapter. Draft conflicts require explicit
replacement authorization. All conflicts are checked before changing draft assets.
An ambiguous upload is reconciled only with a unique matching remote digest and
size; otherwise the draft is retained for inspection. Mismatched acknowledgments
never cause automatic deletion. Final inventory verification rejects extra or
duplicate assets.

## Source identity

Builds record a full lowercase 40-hex checkout commit and credential-free repository
identity only when the tracked checkout is clean and has a usable origin. This is
a checkout claim, not a signature or proof of reproducible builds. Untracked build
inputs are not authenticated by that claim. The CLI requires a claim by default;
`require_commit = false` explicitly permits its absence without manufacturing one.

Before release mutation, the authenticated API verifies the requested repository,
exact `refs/tags/<name>`, Git object types and hashes, bounded annotated-tag peeling,
and commit existence. A present claim must equal the peeled commit. The tag name
and `Release.target_commitish` are never treated as Git-object proof. Contradictory
release metadata is refused.

Missing tags fail unless creation was explicitly enabled and a specific valid
commit was proven in the same repository. Creation is non-force and followed by
exact readback, including lost-acknowledgment reconciliation. A foreign tag winning
the creation race prevents release mutation. Receipts record the resolved ref,
tag-object hash, peeled commit, optional build claim, expected asset hashes, and
separate API-reported asset hashes. Resume checks the prior receipt and remote ref
again before creating anything.

GitHub has no compare-and-publish transaction for a tag and release. Checks before
and after publication detect movement, but cannot eliminate that race or undo a
publication already observed by readers. Use remote tag protection and immutable
releases for that stronger guarantee. The adapter never repairs tags automatically.

## Workflow and action contracts

The generator returns a validation result. Its small typed finalized-producer
choice selects `compose` or `sign`; absent jobs cannot become dependencies.
Every output has a producer. The publisher CLI is built from the same immutable
source revision as the action, before use. The default revision comes from the
generator's Git checkout or packaged VCS metadata; archives without either require
an explicit build revision. A conflicting override is refused.

Plan resolves the requested manual/tag ref to one commit. All later checkouts use
that SHA. Native matrix jobs install their toolchain components and upload exact
file lists, preserving per-profile roots. Composition consumes those roots. Signing
uses conditional Bash/PowerShell steps, then mandatory signature verification.
Attestation and publication download the same finalized artifact. Dry runs suppress
remote attestation and publication. Only the publisher has `contents: write`;
publication credentials do not enter build subprocesses or anonymous downloads.

Action fixes include real directory enumeration for workflow uploads, distinct
`--output-dir` handling, correct sign subcommand option placement and manifest
paths, declared signing inputs, and using the GitHub API token rather than an OIDC
token for attestation uploads. SLSA dependencies retain the workflow event identity
and add the actual recorded build commit for manual dispatch.

Removed internal APIs include suffix-based package collectors and recursive source
location. `build_plan` returns both the validated plan and exact source map;
`generate` returns `Result`. No legacy adapters were retained.

## Verification and limits

Native Windows validation passed 527 affected Rust tests. A final composition-path
hardening change was followed by all 25 publication tests. The affected set includes
`zup-publish`, `zup-publish-github`, `zup-distribute-github`, `zup-artifact`, `zup`,
`zup-bundle`, `zup-compose`, `zup-manifest`, and `zup-signing`, with all features.
Action verification passed 187 tests, type checking, formatting, bundle generation,
metadata checks, and execution of the bundle on Node 24.15.0. Workspace all-target,
all-feature Clippy passed with `-D warnings`. Workspace doctests passed: 1 test
across 45 crate suites, no ignored tests.

Native Linux under WSL ran the same affected package set with the task's locked
dependency graph: 491 tests, 488 passed and 3 failed. All publication tests passed,
including native CLI composition and verified downloads. The failures are in the
unchanged `zup::linux_build` suite: `a_machine_service_project_checks_and_builds`
uses the removed `services.name` field; `a_non_executable_main_is_refused_before_build`
expects a later diagnostic despite an invalid application-main reference; and
`a_public_build_installs_runs_and_uninstalls` fails its ledger-removal assertion.
Those failures were not repaired in this publication change. Native installer
toolchains and the WASI configure fixture were staged on both platforms. No UAC or
machine service mutation was needed for the publication regressions.

The workflow contract test parses and actionlints 16 generated configurations:
signing on/off, attestations on/off, one/multiple targets, and Ubuntu/Windows
composition runners, with custom paths and runner overrides. Both triggers and
dry-run expressions are included. The semantic validator checks dependencies,
outputs, action inputs, pinned tool installation, artifact names and transfer paths,
and the finalized producer shared by attestation and publication.

The multivariant regression checks 11 exact products through staging, plan,
`PublishRequest`, fake API uploads, receipt and repeat publication, then reads all
payloads through the normal digest-verifying GitHub content consumer. A separate
native CLI fixture builds from `zup.toml`, removes the original payload source,
composes matrix bytes, finalizes, dry-runs, publishes twice to the fake API, and
downloads a native object through the verifying cache. Error cases cover damaged
and reordered transport, manifest injection, foreign paths, signing refusal,
repository mismatch, malformed SHAs, nested annotated tags, moved refs, tag
creation collisions, and credential isolation.

All release/tag mutation tests use loopback fake APIs or temporary Git repositories.
No real GitHub release or tag was created. These tests do not prove hosted Actions
runner provisioning, GitHub production consistency, real Sigstore/OIDC exchanges,
or signing with a production certificate. Custom signing tools and their keys are
external prerequisites. Explicitly inventoried TUF metadata can be published, but
this change does not introduce a TUF signing authority or automatically inventory
files emitted by an arbitrary external signer. Such files must be bound by the
release-manifest authority before publication. No power-loss guarantee is claimed.
