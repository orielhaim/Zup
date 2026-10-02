# Guide

Zup separates five concerns that installer systems often mix together:

1. **Project** - `zup.toml` describes the application and its targets.
2. **Installer UI** - a [preset](/presets/) presents install and maintenance state.
3. **Extensions** - [plugins](/plugins/) add computed resources during planning.
4. **Artifacts** - a build turns target profiles into files users can run.
5. **Release** - signing and publishing happen after the build.

Most applications only need the first and fourth parts at the start.

## Current platform support

Zup is designed around target profiles and portable installation intent. The Windows backend is the implementation that ships today. macOS and Linux backends are not available yet.

That distinction matters: `zup.toml`, preset packaging and the public model are not Windows-shaped, but a non-Windows installation target is currently refused at the backend boundary.

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
