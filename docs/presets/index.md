# Presets

A preset is the installer UI: a native Rust program built with GPUI and `zup-sdk`, packaged as `.zupui`.

Zup owns installation state and lifecycle behavior. A preset decides how that state is presented and which valid actions the user can request. The default preset ships with Zup.

## Two users of the preset system

**Application authors** choose a package and fill its settings:

```toml
[ui]
preset = "./aurora.zupui"

[ui.settings]
hero = "Install Acme"
accent = "#695cff"
```

**Preset authors** write Rust against `zup-sdk`, develop against a simulated installer, and package native binaries into `.zupui`.

## What a preset owns

A preset owns:

- window layout and navigation;
- how application identity is presented;
- component and scope controls;
- progress, failure and maintenance presentation;
- preset-specific settings and assets.

A preset does not define files, services, privileges, transactions or update policy. Those stay in the project and installer lifecycle.

## Workflow

```text
zup preset init aurora
    ↓
zup preset dev
    ↓
zup preset pack --build <target>
    ↓
[ui] preset = "./aurora.zupui"
    ↓
zup preview
```

Start with [Use a preset](./use) if you are configuring an application. Start with [Create a preset](./create) if you are authoring one.
