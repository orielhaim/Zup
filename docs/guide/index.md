# Guide

Zup separates five concerns that installer systems often mix together:

1. **Project** - `zup.toml` describes the application and its targets.
2. **Installer UI** - a [preset](/presets/) presents install and maintenance state.
3. **Extensions** - [plugins](/plugins/) add computed resources during planning.
4. **Artifacts** - a build turns target profiles into files users can run.
5. **Release** - signing and publishing happen after the build.

Most applications only need the first and fourth parts at the start.

## Current platform support

Windows and Linux installers build from either host. Linux ships `console` and `headless` runtimes for `linux-x64`; `gui` is Windows-only. There is no macOS backend: a macOS target is refused before the payload is read.

See [Platforms and targets](./platforms) for the exact model.

## Recommended path

Follow this order for a new application:

```text
Quick start
  → Project
  → Platforms and targets
  → Install scope and paths
  → Payload files
  → Components / system integration as needed
  → Check and preview
  → Build
  → Ship
```

Do not start with [Presets](/presets/) unless the default installer UI is insufficient. Do not start with [Plugins](/plugins/) unless the manifest cannot express the resource statically.
