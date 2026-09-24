# zup

A programmable application installer for the modern desktop.

Zup is in very early development. The public API is not available yet.

## Installer frontends

See [GUI, console, and headless installer frontends](docs/frontends.md) for build-time selection, automation output, exit codes, elevation, and Server Core guidance.

## Plugins

See [plugin authoring and runtime architecture](docs/plugins.md) and the [Rust configure example](examples/plugins/configure).

## Windows installer artifacts

`zup build` writes the package into the PE resource section: RCDATA resource 1
contains the bundle index, and each unique compressed payload blob has its own
RCDATA resource. The index is limited to 256 MiB, each resource to 4,294,967,295
bytes, and a package to 65,534 unique blobs. Sign the completed `Setup.exe`
with your normal Authenticode tool after `zup build`; the signature then covers
the package resources.
