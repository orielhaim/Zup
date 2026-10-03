<div align="center">

<img src="docs/public/zup.svg" width="64" alt="Zup">

# Zup

**Cross-platform application installer**

[![Docs](https://shieldcn.dev/badge/docs-zup.orielhaim.com-4f46e5.svg?logo=ri%3ALuBookOpen&logoColor=ffffff)](https://zup.orielhaim.com)
[![License](https://shieldcn.dev/github/license/orielhaim/Zup.svg?color=64748b&logo=ri%3ALuScale&logoColor=ffffff)](https://github.com/orielhaim/Zup#license)

</div>

Zup builds application installers from a single `zup.toml`

Describe your application once - files, components, prerequisites, services, shortcuts, protocols, UI and updates - and Zup turns it into a native installer with the full application lifecycle built in.

The goal is simple: **shipping a desktop application should not require maintaining a different installer stack for every platform.**

## What Zup handles

- Install, upgrade, modify, repair and uninstall
- User and machine-wide installations
- Components and optional features
- Prerequisites
- Files, launchers, services, PATH entries, protocols and file associations
- Custom installer windows through [presets](https://zup.orielhaim.com/presets/)
- Sandboxed extensions through [plugins](https://zup.orielhaim.com/plugins/)
- Offline and thin installers
- Updates, signing and release publishing
- GUI, console and headless installers

## Install

Zup is still pre-release. Until packaged releases are published, build it from source:

```bash
git clone https://github.com/orielhaim/Zup.git
cd Zup

cargo xtask toolchain build
cargo run -p zup -- --version
```

## Quick start

Create a project:

```bash
zup init --name Acme --app-id com.acme.desktop
```

Zup creates a `zup.toml` describing the installer:

```toml
schema = 1

[app]
id = "com.acme.desktop"
name = "Acme"
version = "0.1.0"
main = "acme.exe"

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"
```

Preview the real installer UI against a simulated machine:

```bash
zup preview
```

Validate the project and build it:

```bash
zup check
zup build
```

That's the basic workflow. The manifest grows with the application instead of being replaced by installer-specific scripts.

## Authoring extensions

Zup has two kinds of extension, and both are written against one crate.

A **preset** is the window an installer draws. An ordinary GPUI application, packaged per target:

```bash
zup preset init aurora
cd aurora && zup preset dev
```

```toml
[dependencies]
zup-sdk = { version = "0.1.0", features = ["preset"] }
```

A **plugin** is a declarative extension to what an application installs. Compiled to WebAssembly, answering one question and returning resources Zup installs:

```bash
zup plugin init configure
cd configure && zup plugin build
```

```toml
[dependencies]
zup-sdk = { version = "0.1.0", features = ["plugin"] }
```

Neither project adds anything else to its manifest - not `serde`, not
`schemars`, not a GPUI version, not a Wasm toolchain.

## Documentation

Start with the **[Zup documentation](https://zup.orielhaim.com/)**.

- [Quick start](https://zup.orielhaim.com/guide/quickstart)
- [Project configuration](https://zup.orielhaim.com/guide/project)
- [Components](https://zup.orielhaim.com/guide/components)
- [Presets](https://zup.orielhaim.com/presets/)
- [Plugins](https://zup.orielhaim.com/plugins/)
- [Shipping](https://zup.orielhaim.com/ship/)
- [Manifest reference](https://zup.orielhaim.com/reference/manifest)
- [CLI reference](https://zup.orielhaim.com/reference/cli)

## License

[Apache-2.0](LICENSE)
