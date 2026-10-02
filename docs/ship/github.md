# GitHub Releases

Configure the repository in `zup.toml`:

```toml
[publish.github]
repository = "acme/desktop"
```

Then publish a finalized release:

```bash
zup publish github --release-dir dist
```

Credentials come from the environment or GitHub tooling; they are not stored in the manifest.

## Tags and release notes

GitHub publishing can derive a version tag or use project-specific tag/notes configuration under `[publish.github]`.

Keep release policy in the manifest when it must be shared by local and CI release paths. Keep secrets outside it.

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
