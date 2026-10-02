---
layout: home
title: Zup
titleTemplate: false
hero:
  name: Zup
  text: Build desktop installers from one manifest
  tagline: One manifest for targets, installer UI, plugins, signing and releases. Cross-platform, with Windows shipping today.
  actions:
    - theme: brand
      text: Get started
      link: /guide/quickstart
    - theme: alt
      text: GitHub
      link: https://github.com/orielhaim/Zup
---

<div class="zup-home">

## One project

`zup.toml` is the application contract. It names the app, target profiles, payload, install policy and system integration. Presets own the installer window. Plugins add computed installation resources. Shipping configuration produces signed, publishable releases from the same project.

<div class="zup-flow">zup.toml → check → preview → build → sign → publish</div>

## Public surfaces

<div class="zup-surfaces">
  <a class="zup-surface" href="/guide/project">
    <h3>Manifest</h3>
    <p>Targets, files, components, prerequisites and install policy.</p>
  </a>
  <a class="zup-surface" href="/presets/">
    <h3>Presets</h3>
    <p>Native installer UIs written in Rust with GPUI and the Zup UI SDK.</p>
  </a>
  <a class="zup-surface" href="/plugins/">
    <h3>Plugins</h3>
    <p>Planner extensions that generate files and declare installation resources.</p>
  </a>
  <a class="zup-surface" href="/ship/">
    <h3>Shipping</h3>
    <p>Artifacts, external code signing, GitHub Releases, static hosting and updates.</p>
  </a>
</div>

## Small by default

A project needs four sections: application identity, a build target, install scope and the schema version. Add components, presets, plugins or release configuration only when the application needs them.

```toml
#:schema https://zup.orielhaim.com/schema/zup.toml.json

schema = 1

[app]
id = "com.acme.desktop"
name = "Acme"
version = "1.0.0"
main = "Acme.exe"

[build.targets.windows]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Acme"
```

Start with the [quick start](/guide/quickstart). Use the [manifest reference](/reference/manifest) when you need the exact surface.
</div>
