# Artifacts

Targets describe native variants. Artifacts describe files users download.

Keep those concepts separate.

## Default

With no `[build.artifacts]`, Zup builds one offline installer per selected target. This is the smallest release model.

## Compact runtime templates

When building Zup from source, prepare distribution templates once:

```bash
cargo xtask toolchain build --profile installer
zup --toolchain target/installer/toolchain/0.0.1 build
```

The `installer` profile optimizes runtimes and native presets for size. It retains
plugin execution, recovery, services, and the other supported lifecycle operations.
GUI installers compress their native preset; Console and Headless carry no preset.
The developer CLI, tests, preview, and preset development keep their ordinary
profiles. `zup build` composes cached templates without compiling the engine.

Use `cargo xtask toolchain build --profile dev` when changing the runtime itself.
For a reproducible size breakdown, run `scripts/ci/installer-size.py` with
`--cli`, `--toolchain`, and `--output`. The size-only mode in Full Verification
can measure a selected revision on native Windows and Linux runners.

See [measured installer sizes](installer-size.md) for the frontend and component breakdown.

## Declare an artifact

```toml
[build.targets.windows-x64]
source = { directory = "dist/x64" }

[build.targets.windows-arm64]
source = { directory = "dist/arm64" }

[build.artifacts.windows]
targets = ["windows-x64", "windows-arm64"]
```

Here `windows` names the artifact, not a target. The artifact is one Windows
installer covering both architectures; each target still builds on its own too.

Fields:

| Field | Values | Meaning |
| --- | --- | --- |
| `kind` | `single`, `universal` | one target or target selection at launch |
| `mode` | `offline`, `thin` | carries content or acquires release content |
| `targets` | profile IDs | variants included by the artifact |
| `channel` | string | follow a release channel instead of a fixed version |
| `output` | path | explicit output name |

## Offline

An offline artifact carries the content it needs. Use it when installation must work without an origin being reachable.

## Thin

A thin artifact is a bootstrapper. It carries release trust information and acquires the matching release/runtime/content from the configured repository.

Thin artifacts require update/repository configuration. With `scope = "either"`,
the launcher asks for a scope before downloading the runtime. Automation must pass
`--scope user` or `--scope machine`. Component flags are forwarded to the runtime,
which selects and verifies the required payload before installation.

## Universal

A universal artifact can contain several target variants and select the matching one when launched. This is useful when one download should cover x64 and ARM64.

Run it like the installer it stages: `install`, `upgrade`, `modify`, `repair`, `update`, and `uninstall` travel to the native runtime with their selection and reporting options. Use `upgrade` to apply a newer artifact to an existing installation. `--plain` selects line prompts; `--non-interactive` and `--yes` suppress prompts. The runtime independently validates the forwarded choices.

Do not make every release universal by default. One file per target is easier to inspect and distribute when a single download is not a product requirement.
