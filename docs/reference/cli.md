# CLI

The developer CLI operates on projects and releases. Installed applications use the generated installer/maintenance executable instead.

## Project authoring

| Command | Purpose |
| --- | --- |
| `zup init` | create a project |
| `zup check` | validate manifest and build inputs |
| `zup doctor` | report build readiness |
| `zup plan` | show installation plan without changing the machine |
| `zup preview` | open the application installer UI on a simulated machine |
| `zup build` | build configured artifacts |
| `zup artifact inspect <file>` | inspect a built artifact |
| `zup fmt` | format `zup.toml` preserving comments |
| `zup schema` | print/write authoritative JSON Schema |
| `zup completions <shell>` | emit shell completions |

Common project selection flags include `--manifest` and repeatable `--target`. Commands that support machine output accept `--format` where documented by `--help`.

## Preset authoring

| Command | Purpose |
| --- | --- |
| `zup preset init <name>` | create a preset project |
| `zup preset dev` | run a preset against a simulated installer |
| `zup preset pack` | package target binaries into `.zupui` |
| `zup preset inspect <file>` | inspect a preset package |

## Plugin authoring

| Command | Purpose |
| --- | --- |
| `zup plugin init <name>` | create a plugin project |
| `zup plugin build` | compile for wasm and componentise into the plugin Zup loads |

Both generate a project whose only dependency is `zup-sdk`. `zup plugin build`
runs Cargo for `wasm32-unknown-unknown` and componentises the result; the author
installs no Wasm tooling and vendors no WIT.

## Release

| Command | Purpose |
| --- | --- |
| `zup sign prepare` | prepare signing plan |
| `zup sign verify` | verify signatures and finalize release |
| `zup publish stage` | stage a static release tree |
| `zup publish github` | publish to GitHub Releases |
| `zup ci github generate` | generate release workflow |
| `zup ci github check` | check committed workflow freshness |

## Toolchain

`zup toolchain` manages the Zup runtime/preset components used to compose installers. Normal application projects should not need to pass hidden runtime/dispatcher overrides.

## Exact flags

Use command help as the authoritative flag reference for the installed Zup version:

```bash
zup --help
zup build --help
zup preset pack --help
zup plugin build --help
zup publish github --help
```

The guides document stable workflows rather than duplicating every parser flag.
