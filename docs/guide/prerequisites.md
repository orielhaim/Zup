# Prerequisites

Prerequisites require software before application installation. Packages can be embedded or downloaded with SHA-256 verification; `detect_only` checks without installing. Requirements can follow components, conditions, and target profiles. See the [manifest reference](/reference/manifest#prerequisites) for all fields.

## Linux packages

```toml
[[prerequisites]]
id = "curl"
name = "curl"
requirement = { kind = "native_package", id = "curl", minimum_version = "8.0" }
package = { type = "repository" }
```

Use `package = { type = "detect_only" }` to require an installed package. Omit `minimum_version` to check presence alone. Package names vary by distribution.

Supported providers are APT, DNF with RPM 4.16+, APK, and Pacman. Versions use native package-manager comparisons, including epochs and distribution revisions. Direct `.deb`, `.rpm`, and `.apk` artifacts are unsupported.

Repository installation uses configured repositories and signature policy. Root is required; interactive installation can request polkit authorization. Offline installation requires cached packages and metadata. Lock and repository failures retain native diagnostics. Zup never removes package-manager locks or shared packages during rollback or uninstall.

For machine deployments, the separately authorized bootstrap handles repository installation before the privileged worker runs. The worker derives active prerequisites from the trusted installer, checks them with read-only native package queries before changing machine state, and checks them again at execution. `detect_only` is a gate: it never installs anything. These checks apply to install, upgrade, downgrade, modify, and repair; cleanup, uninstall, and rollback do not require the current installer’s prerequisites. Recovery settles an existing Zup transaction and does not install prerequisites.

Package-manager operations are outside Zup’s deployment transaction. If a package installation succeeds and the application deployment later fails, the system package remains installed. Rollback and uninstall leave shared packages in place.

Cancellation prevents launch. Once installation starts, cancellation waits for the package transaction to finish. After 15 minutes, Zup reports recovery-required and leaves the package manager running. Recovery waits for the recorded process group to exit, then checks the requirement again. An uncertain launch or unsatisfied interrupted transaction requires native package-manager inspection or repair.

To reproduce the worker’s satisfied `detect_only` path in a disposable Debian container, run `scripts/verify-linux-prerequisite-worker.sh` from a machine with Docker. The check queries the already-installed `bash` package and does not run a package installation command.
