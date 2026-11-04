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

## Linux native packages

Use a distribution package name and a native minimum version:

```toml
[[prerequisites]]
id = "curl"
name = "curl"
requirement = { kind = "native_package", id = "curl", minimum_version = "8.0" }
package = { type = "repository" }
```

Use `package = { type = "detect_only" }` to require an existing package without installing it. Omit `minimum_version` to check presence alone. Package names differ between distributions; use target conditions or separate manifests when necessary. Native package prerequisites use the package manager's default architecture and accept no artifact installer options.

Zup discovers Debian/Ubuntu APT, Fedora/RHEL DNF with RPM 4.16 or newer, Alpine APK, and Arch Pacman from `os-release` and their native executables. It asks the native database about installed packages and compares versions using native dependency semantics. Epochs and distribution revisions are not SemVer. Other distributions, immutable variants without these tools, and direct `.deb`, `.rpm`, or `.apk` artifacts are refused.

Repository policy uses the administrator's configured repositories and package-manager signature policy. Zup does not run a separate metadata-refresh operation, remove lock files, or disable signature checks. The native manager may refresh expired metadata during installation. Offline installation works only when the native manager already has the required metadata and package content. Lock contention and repository failures report native diagnostics. Noninteractive installation needs root; interactive runs can request polkit authorization through `pkexec`.

Package installations are shared system changes. Application rollback and uninstall never remove them. Bootstrap writes its operation state durably before starting a package manager, then re-detects the requirement before allowing application installation. Cancellation is checked before each launch and after the active package transaction finishes. An interrupted operation is re-detected on retry. If it is still unsatisfied, Zup requires native package-manager repair instead of automatically replaying an ambiguous operation.
