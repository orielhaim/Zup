# Prerequisites

Prerequisites describe software that must already be present before the application can be installed.

A prerequisite has three parts:

1. **Requirement** - what must be true.
2. **Package** - what Zup can run when it is not true.
3. **Installer policy** - arguments, accepted exit codes and required privilege.

## Runtime requirement

```toml
[[prerequisites]]
id = "vc-runtime"
name = "Visual C++ Runtime"
requirement = { kind = "runtime", id = "vc-runtime" }
package = {
  type = "remote",
  url = "https://example.invalid/vc-runtime.exe",
  sha256 = "...",
  filename = "vc-runtime.exe"
}
```

Requirements can represent a runtime, an installed package, or a file version. Version constraints are optional where supported.

## Embedded and remote packages

An embedded package ships with the installer. A remote package is fetched from its declared URL and verified against its SHA-256 digest.

Use embedded packages when offline installation matters. Use remote packages when the dependency is too large or changes independently.

## Scope and selection

A prerequisite may be tied to a component, a `when` condition, or target profiles. This keeps prerequisites aligned with the part of the application that actually needs them.

The exact requirement and package shapes are listed in the [manifest reference](/reference/manifest#prerequisites).
