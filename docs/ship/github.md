# GitHub Releases

Configure the repository in `zup.toml`:

```toml
[publish.github]
repository = "acme/desktop"
```

Then publish a finalized release:

```bash
zup publish github --release-dir dist --web dist/web --packages dist/packages
```

Credentials come from the environment or GitHub tooling; they are not stored in the manifest.

The release manifest records the exact installers, web documents, native objects,
package descriptors, and physical transport shards. Publishing verifies their sizes,
hashes, and relationships before contacting GitHub. Foreign or missing files are errors.

## Tags and release notes

GitHub publishing can derive a version tag or use project-specific tag/notes configuration under `[publish.github]`.

Build from a clean tracked Git checkout with an `origin` matching the configured
repository. The build records the full checkout commit. The requested remote tag
must peel to that commit; annotated tags are supported. Missing tags fail unless
`create_tag = true` explicitly permits creating that exact tag at the proven commit.
Zup never moves an existing tag. Correct the tag or rebuild from its commit, then retry.

`require_commit = false` permits a release without recorded build-source evidence.
It does not authenticate that release's source. Existing remote tags are still
resolved; Zup cannot create a tag without an explicit commit.

Published assets cannot be replaced. Draft conflicts require explicit
`replace_conflicts = true`. Keep a receipt with `--receipt publication.json` when
resuming: it records the resolved tag object, commit, and asset identities. An API
asset digest, when available, is distinguished from the expected local digest.
Name and size alone do not establish remote byte identity.

GitHub has no atomic compare-tag-and-publish operation. Zup checks the tag before
and after publication, but repository tag protection and immutable releases are
needed to prevent external movement during or after those checks.

## GitHub as a content host

A project can also configure clients to fetch release content from GitHub release assets:

```toml
[distribution]
host = "github"
channel = "stable"
```

Thin installers need anonymous read access to their release content. A private repository cannot serve that content to a credential-free bootstrapper; use a public release source or a static origin for that case.

## CI

For repeatable releases, generate and commit the workflow described in [Release CI](./ci).
