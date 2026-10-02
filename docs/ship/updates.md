# Updates

Updates bind an installed application to a release repository and channel.

```toml
[updates]
repository = "https://downloads.example.com/acme"
channel = "stable"
root = "keys/root.json"
```

`repository` is the client-facing release origin. `channel` selects the release stream. `root` is the trusted root material included by the build.

## Channels

Use channels for release policy such as `stable` or `beta`. The installed maintenance UI can check the configured channel when updates are enabled.

A channel-following artifact is different from a version-pinned artifact: one follows the channel's current release; the other always names the exact version it was built for.

## Downgrades

Zup rejects a lower application version as an update. An equal version is handled as maintenance/modify behavior rather than a downgrade.

## Thin installers

Thin artifacts use the update/release repository to acquire the runtime and content they do not carry. Configure updates before declaring a thin artifact.

## Publish atomically

Use Zup's staging/publishing flow rather than editing update metadata by hand. Installed clients rely on the published release metadata and trusted root to decide what content is acceptable.
