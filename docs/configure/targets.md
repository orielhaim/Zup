# Targets

A target profile is one buildable variant: a target triple, and the directory
its payload comes from. Declare at least one.

```toml
[build]

[build.targets.windows]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }
```

`zup build` with no `--target` builds every declared profile. One profile means
you never have to name it.

## Profile fields

| Key | Required | Meaning |
| --- | --- | --- |
| `target` | yes | A target triple |
| `source` | yes | `{ directory = "path" }` - the payload for this target |
| `frontend` | no | `gui`, `console` or `headless`. Inherits the top-level `frontend` |
| `install` | no | A full `[install]` table. Inherits the top-level one |

A profile name is any non-blank string. `windows`, `windows-x64` and
`default` are all fine; the name is what you pass to `--target`.

## Target triples

A triple is `<arch>-<vendor>-<os>`. The manifest is platform-neutral: it names a
target, and the backend for that target resolves the install locations,
launchers, services and file associations against that platform's conventions.
Nothing in `zup.toml` is Windows-specific.

Zup normalizes what it accepts:

- A leading `x64-` is rewritten to `x86_64-`, so `x64-pc-windows-msvc` and
  `x86_64-pc-windows-msvc` are the same triple.
- `arm64-pc-windows-msvc` is stored canonically as `aarch64-pc-windows-msvc`.

Two profiles cannot resolve to the same canonical triple. That is an error, not
a warning - it would mean two names for one native variant.

::: tip A triple that names a platform Zup has no backend for
Zup refuses it before reading your payload, with
`unsupported backend for target ...: backend not implemented`. The refusal
happens at the backend boundary, so nothing is written and a typo in an
architecture is reported before you have built anything. A target whose platform
is implemented but whose build host is not gets a separate message naming the
host requirement.
:::

## Frontend

The installer experience. Set it once for every target:

```toml
frontend = "gui"
```

| Value | What the user sees |
| --- | --- |
| `gui` | The graphical installer window. The default |
| `console` | A console front end |
| `headless` | No interface. Progress on the console, no prompts |

`gui` and `console` need a preset package. `headless` does not - it has no
window - so it is the one frontend that builds on a machine with no toolchain
preset.

A profile can override the top-level value, and `--frontend` overrides the
profile.

## Multiple architectures

```toml
[build]

[build.targets.x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/x64" }

[build.targets.arm64]
target = "aarch64-pc-windows-msvc"
source = { directory = "dist/arm64" }
```

Building one of them:

```bash
zup build --target x64
```

`--target` accepts a profile name or a canonical triple, and is repeatable. A
profile name wins over a triple when both would match.

## Artifacts

A target profile is a native variant. An artifact is a file a user downloads.
Declaring artifacts is optional: a project that declares none builds one
installer per target, which is the simplest thing that works.

```toml
[build.artifacts.windows]
targets = ["x64", "arm64"]
```

| Key | Default | Meaning |
| --- | --- | --- |
| `targets` | required | The profiles this artifact includes |
| `kind` | `universal` | `universal` carries every target and picks one at run time. `single` carries exactly one |
| `mode` | `offline` | `offline` contains every byte. `thin` fetches the rest by digest |
| `channel` | none | The [update channel](/ship/updates) this artifact follows |
| `output` | derived | The output file name |

Without `output`, the name is derived from the kind and the channel:

| Kind | Channel | Name |
| --- | --- | --- |
| `universal` | none | `Acme-Windows-Setup.exe` |
| `single` | none | `Acme-Setup.exe` |
| `universal` | `stable` | `Acme-Windows-stable-Setup.exe` |
| `single` | `stable` | `Acme-stable-Setup.exe` |

This is a different rule from a [per-target build](#multiple-architectures),
which names its output after the profile rather than the kind. A version is not
part of either name.

A `universal` artifact with no channel is labelled with an exact version and
always installs that version; one with a channel installs whatever the channel
currently says, which is a different promise and a different file name.

Building declared artifacts:

```bash
zup build --artifact windows
zup build --universal        # one artifact from every selected target
```

`zup build` with no flags builds every declared artifact, or one installer per
target when none are declared.

The output name for a per-target build follows the target count, not the kind:

| Selected targets | Name |
| --- | --- |
| One | `Acme-Setup.exe` |
| Several | `Acme-Setup-<profile>.exe` |

Next: [install location](/configure/install).
