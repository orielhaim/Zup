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

## Windows native inventory

Windows uses the same detector for bootstrap assessment, execution checks, and recovery of an interrupted external installer. Missing and incompatible installations remain distinct. Access refusal, corrupt inventory, unsupported runtime IDs, and invalid versions stop detection with an error; they do not trigger another installer run. Existing protected acquisition, worker authorization, cancellation and reboot handling still apply.

MSI requirements use an exact braced ProductCode GUID. Machine deployments query only the machine context. User deployments accept machine installations or installations for the executing user in either managed or unmanaged context. Other users' installations never satisfy a requirement, including when the process is elevated. Zup queries each allowed context with `MsiGetProductInfoExW`; only product state `5` counts as installed. Advertised state `1` is missing. Version and state must come from the same context. Architecture constraints use the registered cached package's summary Template through `MsiGetSummaryInformationW`; failure to read that evidence is an error. `any` does not impose a package architecture constraint.

MSI strings use the API's returned UTF-16 length excluding NUL, allocate one extra code unit, validate UTF-16, and retry bounded growth. Unknown products are absent; other API failures preserve their Win32 status. A product disappearing after detection begins is an error.

.NET detection enumerates registered versions in `HKLM\SOFTWARE\dotnet\Setup\InstalledVersions\<architecture>\sharedfx\Microsoft.NETCore.App` or `Microsoft.WindowsDesktop.App`. All architectures use the 32-bit registry view, matching Microsoft's installer schema. Version names must be valid semantic versions with DWORD marker `1`. The highest actually registered matching version satisfies the requirement, so side-by-side 8 and 10 satisfy `8.*`. No registered versions is missing; registered versions with no match is incompatible. This contract covers system installer registrations, not private ZIP deployments or arbitrary `DOTNET_ROOT` directories.

`current` means the architecture of the Zup process. Explicit x86, x64 and arm64 inventories remain separate, including under emulation. `any` searches supported architectures using `GetNativeSystemInfo`, rather than inferring OS bitness from the process. VC++ v14 also requires its `Installed` marker. WebView2 Evergreen uses EdgeUpdate's platform registration in the 32-bit machine registry view, with current-user registration as a fallback for user deployments. Use `target = "any"` for Evergreen: this registration does not prove an executable architecture, so other target constraints are refused rather than guessed. A zero Evergreen version means uninstalled.

Ranges use `semver::VersionReq`. Windows numeric four-part versions retain the fourth component as semantic-version build metadata, for example `131.0.2903.112` becomes `131.0.2903+112`. Semver ranges compare the first three components and ignore build metadata; revision-specific constraints are not supported. Malformed or extra components are rejected. A leading `v` is accepted for VC++ registration. .NET versions use strict semver, including its prerelease matching rules.

Sources: [MSI string contract](https://learn.microsoft.com/en-us/windows/win32/api/msi/nf-msi-msigetproductinfoexw), [MSI contexts](https://learn.microsoft.com/en-us/windows/win32/msi/determining-installation-context), [.NET inventory schema](https://github.com/dotnet/arcade/blob/main/src/Microsoft.DotNet.Build.Tasks.Installers/build/wix5/product/registrykeys.wxs), and [installed .NET runtimes](https://learn.microsoft.com/en-us/dotnet/core/install/how-to-detect-installed-versions).
