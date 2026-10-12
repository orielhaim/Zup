# Prerequisites

Prerequisites describe software that must already be present before the application can be installed.

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

Requirements name a runtime, an installed package, or a file version, with optional version constraints. An embedded package ships with the installer; a remote package is fetched from its URL and verified against its SHA-256 digest.

A prerequisite may be tied to a component, a `when` condition, or target profiles. The exact shapes are in the [manifest reference](/reference/manifest#prerequisites).
