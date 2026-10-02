# Install Zup

Zup is cross-platform: the manifest, the CLI and the build pipeline are
platform-neutral, and each target's backend resolves a manifest's intent against
that platform's conventions.

Zup ships as one directory. It contains the `zup` executable and the binaries
that `zup build` composes an installer from. Unpack it and the CLI works.

```text
zup/
  zup.exe
  zup-toolchain.json
  toolchain/
    0.0.1/
      zup-setup-gui-x86_64-pc-windows-msvc.exe
      zup-setup-gui-x86_64-pc-windows-msvc.exe.zup-toolchain.json
      ...
```

Put the `zup` directory somewhere on your `PATH` and verify it:

```bash
zup --version
zup toolchain status
```

`zup toolchain status` reports whether the binaries a build composes from were
found, and where. A ready toolchain ends with:

```text
ready: every component a build needs is present and verified
```

## How Zup finds its own binaries

`zup build` does not compile an installer. It composes one out of binaries that
were built once and shipped beside the CLI: a runtime template for the target's
architecture and installer experience, a launcher, and the preset that draws the
window. Every Zup release carries those files, and every one of them is
identified by a digest in `zup-toolchain.json`.

Zup looks for them in this order, and uses the first match:

1. `--toolchain <DIR>`, or the `ZUP_TOOLCHAIN` environment variable
2. `<state-root>/toolchain/<version>/` - the per-machine cache
3. `toolchain/<version>/` beside `zup.exe`, then `toolchain/`

Each file carries a `.zup-toolchain.json` descriptor beside it naming the Zup
version, target and installer experience it was built for, plus its digest. Zup
checks the descriptor and the file's own bytes before using it, and a component
built by a different Zup version is refused rather than used.

## Building Zup from source

To work on Zup itself, or to produce the release directory above:

```bash
git clone https://github.com/orielhaim/zup
cd zup

# The binaries a build composes from, staged beside `zup` for `cargo run`.
cargo xtask toolchain build

cargo run -p zup -- --help
```

`cargo xtask toolchain build` is the step people miss. Without it, `cargo run -p
zup -- build` fails with `zup.toolchain.component_missing`, because the CLI is
there but the binaries it composes with are not.

To assemble a complete release directory - the CLI, this version's toolchain, and
the index naming every file with its digest:

```bash
cargo xtask toolchain package
```

The result is written to `target/release-material/<version>/` and is a directory
you can zip and hand to somebody.

## Installing into the machine cache

To copy a release directory's components into the per-machine cache instead of
leaving them beside the executable:

```bash
zup toolchain install path/to/release
```

The argument is a directory holding `zup-toolchain.json`, not a URL. Zup
verifies every file the index names, copies them, then re-verifies the copy by
resolving out of the cache the way a build will. A refused install writes
nothing.

A toolchain is usable only by the Zup release that produced it, so two Zup
versions on one machine each keep their own. When an upgrade makes the old one
unreachable:

```bash
zup toolchain clean            # remove other versions, keep this one
zup toolchain clean --dry-run  # report sizes, remove nothing
zup toolchain clean --all      # remove this one too
```

## Requirements

- A build host that can run the backend for the targets you build. Each backend
  states its own host requirements, and `zup doctor` reports them.
- A Rust toolchain, only if you are building Zup from source.

Next: [create an installer](/getting-started/first-installer).
