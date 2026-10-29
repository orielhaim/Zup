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

Zup rejects a lower application version as an update. An equal version is not an available update. Use modify or repair for the installed release.

## Installed Linux applications

GUI maintenance offers Check for updates. Console also offers it in the maintenance menu. Console and Headless accept:

```sh
./Acme-Setup update check --scope user --output json
./Acme-Setup update --scope user --yes --output jsonl
```

Checking never installs anything. An update preserves the installed scope, directory, and component choices. It authenticates the channel with TUF and verifies the complete selected release before installation. Machine-wide updates acquire content in the invoking user's cache, then require native PolicyKit authorization for execution.

`--source DIRECTORY` adds a local content source; it does not replace repository authentication. Ctrl+C cancels acquisition without changing the installation. After installation starts, cancellation follows the normal transaction and rollback rules.

The maintenance copy retains verified content for every component, plugins, and the GUI preset. Modify, repair, uninstall, and recovery remain available without a connection. Builds automatically select an online Linux runtime when `[updates]` is configured. Applications without updates use an offline runtime with no HTTP or TUF dependencies.

## Thin installers

Thin artifacts use the update/release repository to acquire the runtime and content they do not carry. Configure updates before declaring a thin artifact.

## Publish atomically

Use Zup's staging/publishing flow rather than editing update metadata by hand. Installed clients rely on the published release metadata and trusted root to decide what content is acceptable.
