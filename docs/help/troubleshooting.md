# Troubleshooting

Start with the command that owns the failing stage. Do not debug a build by guessing at installer internals.

## Manifest or payload errors

```bash
zup check
```

Use this first for unknown keys, invalid component/target references, missing files, invalid plugin components and preset-setting errors. Unknown manifest keys are always rejected, including ones added for a future release.

Keep the schema directive in `zup.toml` so the editor catches many of these before the CLI runs.

`[install.directory]` must cover every scope `scope` allows; `either` needs both `user` and `machine`.

## Build host errors

```bash
zup doctor
```

Use `doctor` when the project is valid but cannot build on the current machine. It is read-only: it reports missing runtime templates and unsupported targets as checks rather than errors.

A triple no backend answers for (such as `aarch64-apple-darwin`) is refused at the backend boundary before the payload is read. `arm64-` rewrites to `aarch64-` and `x64-` to `x86_64-`.

## Missing toolchain

```text
no runtime template for `gui` on x86_64-pc-windows-msvc was found.
```

```bash
zup toolchain status
```

It reports every component, whether it was found, and where it came from. A toolchain belongs to the Zup release that produced it; components from another version are refused. From a Zup checkout, `cargo xtask toolchain build` stages one.

## The plan is wrong

```bash
zup plan --scope user
```

Add `--enable` / `--disable` to reproduce component choices. Fix the manifest or plugin that declares the wrong resource; do not patch the generated installer.

## The window is wrong

Application author:

```bash
zup preview
```

Preset author:

```bash
zup preset dev
```

Use `preview` to test a project's selected preset and settings. Use
`zup preset dev` to iterate on preset source.

## Preset settings are rejected

Inspect the package:

```bash
zup preset inspect path/to/preset.zupui
```

Then compare `[ui.settings]` with the preset's schema. Settings belong to the
selected preset; they are not global Zup options.

## A plugin does not build

```bash
zup plugin build
```

Run it from inside the plugin project, or pass `--project <dir>`. The project
needs `zup-sdk` with the `plugin` feature and `crate-type = ["cdylib"]`; `zup
plugin build` compiles for `wasm32-unknown-unknown` and componentises the result,
so there is nothing else to install and nothing to configure.

Then run `zup check`. If the plugin loads but returns the wrong resources, reduce
it to one context-dependent resource and inspect `zup plan`.

## Signing is refused

An untrusted chain means a development certificate: `zup sign prepare --release-dir dist --allow-untrusted-chain --allow-missing-timestamp`, and the relaxation travels in `zup-signing.json`.

A `universal` artifact contains a runtime: sign the runtime first, re-compose, then sign the artifact. Signing only the outer `.exe` is not enough.

`zup sign verify --allow-unsigned` finalizes an unsigned internal release. Linux releases always finalize this way; their authenticity is the artifact digest and release identity.

## Publishing is refused

No credential: set `GH_TOKEN` or `GITHUB_TOKEN`, or run `gh auth login`. A manifest cannot carry a token.

A differing asset on a draft is a conflict: read the diff first, then `--replace-conflicts`. On a published release, replacing is refused.

## Release workflow is stale

```bash
zup ci github check
```

Regenerate and review the diff:

```bash
zup ci github generate --force
```

## Need exact parser behavior

Use the version on your machine:

```bash
zup <command> --help
zup schema
```

Those are authoritative for CLI flags and manifest shape.
