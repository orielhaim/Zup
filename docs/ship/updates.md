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

Use channels for release policy such as `stable` or `beta`. The installed maintenance UI can check the configured channel when updates are enabled. Channels and mirrors under the same application trust root share repository metadata history. Independent repositories need independent roots.

A channel installer selects the current authenticated release. A release installer pins the digest of its published release document. Two builds with the same displayed version remain distinct releases.

## Downgrades

Zup rejects a lower application version as an update. A different authenticated build with the same displayed version can be installed as an update. Repair and Modify use the installed release's exact source document, even after the channel advances.

## Installed applications

GUI maintenance offers Check for updates. Console also offers it in the maintenance menu. Console and Headless accept:

```sh
./Acme-Setup update check --scope user --output json
./Acme-Setup update --scope user --yes --output jsonl
```

Checking never installs anything. An update preserves the installed scope, directory, and component choices. It authenticates the channel with TUF and verifies the complete selected release before installation. Machine-wide updates acquire content in the invoking user's cache, then require native PolicyKit authorization for execution.

`--source DIRECTORY` adds a local content source; it does not replace repository authentication. Ctrl+C cancels acquisition without changing the installation. After installation starts, cancellation follows the normal transaction and rollback rules.

Uninstall uses protected installation ownership. Repair and Modify preserve the recorded release identity and require its verified content. If content must be acquired again, the repository must still authorize the recorded document and its dependencies. Publishers must retain those immutable TUF targets while installations can require them. Builds automatically select an online Linux runtime when `[updates]` is configured. Applications without updates use an offline runtime with no HTTP or TUF dependencies.

## Thin installers

Thin artifacts use the update/release repository to acquire the runtime and content they do not carry. Configure updates before declaring a thin artifact.

## Publish releases

Staging writes immutable release, catalog, and variant documents under `documents/sha256`. Only `releases/<channel>.json` changes when the channel advances. Publish the content and signed TUF targets before updating the channel metadata. Retain the immutable targets for supported installed releases.

Windows workers execute the installed or supplied installer and treat the selected release image as authenticated data. Caller-owned images, caches, overlays, and local repositories are accessed with the invoking user's permissions. Installed applications run directly from their selected deployment without a Zup process.
