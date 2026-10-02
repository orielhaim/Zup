# Ship

A release is a sequence of explicit operations:

```text
build → sign externally → verify → publish
```

Zup does not need access to your private signing key. It prepares the files that need signatures, verifies the result, and publishes the release description and artifacts.

## Choose an artifact model

Start with an offline installer for one target. Add universal or thin artifacts only when the release needs them.

See [Artifacts](./artifacts).

## Choose a publisher

For GitHub-hosted projects, [GitHub Releases](./github) is the shortest path. Zup can also stage a static release tree for an HTTP origin or object store.

## Keep CI reviewable

`zup ci github generate` writes a release workflow from project configuration. Commit the workflow and review it like source code.

See [Release CI](./ci).
