# App metadata

`[app]` is who the application is. The name, version and publisher here are what
the installer window shows and what the installed application's maintenance tool
shows later.

```toml
[app]
id = "com.acme.desktop"
name = "Acme"
version = "1.4.0"
publisher = "Acme Inc"
description = "A tool for doing the thing."
main = "acme.exe"
icon = "assets/icon.svg"
```

| Key | Required | Meaning |
| --- | --- | --- |
| `id` | yes | Reverse-DNS identifier. Stable across versions |
| `name` | yes | The display name |
| `version` | yes | Semver. The lifecycle refuses a same-version upgrade |
| `publisher` | no | Shown in the window and in the maintenance tool |
| `description` | no | One line, shown under the name |
| `main` | no | The file the platform launches, relative to the install directory |
| `icon` | no | A path, or a table with `source` and `padding` |

## id

A reverse-DNS identifier, and the one thing here that must never change. The
installed application, its content cache, its `PATH` entry and its update
rollback memory are all keyed by it. Changing it produces a different
application, not a new version of this one.

## version

A semver string. Zup compares releases by release digest where both sides have
one, and by version otherwise - so rebuilding the same version is not an update.
The lifecycle would refuse it as a same-version upgrade anyway.

## main

The file the platform launches, relative to the install directory. A template is
allowed, so `${app.name}.exe` works. Zup does not create a Start menu entry for
it by itself; declare a [launcher](/configure/registration#launchers).

## icon

Accepted formats are decided by extension: `svg`, `png`, `webp`, `ico`, `icns`.

The short form takes the path and leaves no padding:

```toml
[app]
icon = "assets/icon.svg"
```

The table form adds padding, which shrinks the image inside its canvas. It
leaves a fraction of the canvas empty on each side, from `0.0` to just under
`0.5`:

```toml
[app.icon]
source = "assets/icon.svg"
padding = 0.10
```

You do not choose output sizes. Each backend derives the right icon artifacts
for its platform from the one source: a `.ico` at 16, 24, 32, 48, 64, 128 and 256
pixels for a Windows target, an `.icns` for macOS, a themed icon tree for Linux. A
raster source is fitted onto a transparent square with its aspect ratio
preserved; an SVG is rasterized at each size.

Omitting `icon` uses the built-in Zup mark. A `source` that does not exist is an
error at build time, not a silent fallback.

Next: [targets](/configure/targets).
