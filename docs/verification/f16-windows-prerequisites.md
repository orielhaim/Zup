# F16 Windows prerequisite detection

Validated on native Windows x64, starting at F15 `131e51d1ffa418635cc0d9fecb0061ccb5ca5cbd`.

## Causes and policy

The MSI helper confused output length with buffer capacity, then removed a real character. The native fixture reproduced length 5 for `1.0.0` and `ERROR_MORE_DATA` with the old five-unit allocation before implementation. The replacement uses official statuses, a bounded capacity including NUL, exact output length, strict UTF-16, and bounded retries.

.NET used the wrong framework path and registry view and read one version. Microsoft's installer writes architecture-specific version-name/DWORD markers in the 32-bit view. Detection enumerates those entries and selects a matching installed version. This host has NETCore 6, 8, 9 and 10 and Desktop 8, 9 and 10; the read-only host comparison checks every reported version.

The existing `DetectionResult` contract represents satisfied, missing and incompatible inventory. Native query failures remain `BootstrapError::Requirement` errors, including invalid inventory and unsupported constraints. MSI contexts come from the bound bootstrap scope, not caller declarations about an installation. Machine scope excludes per-user installations. Current-user queries explicitly test managed and unmanaged contexts; no foreign-user enumeration or registry inference is used. An installed ProductCode with only a different package architecture is an error rather than permission to reinstall the same identity.

See [the prerequisite guide](../guide/prerequisites.md#windows-native-inventory) for API, version and architecture policy. Evergreen now requires target `any` because EdgeUpdate's platform registration does not establish executable architecture.

## Native evidence

- A generated MSI installs a registry marker only under `Software\Zup\Tests\<random ProductCode>`. Install/query/uninstall covers per-user unmanaged and, with approved UAC, per-machine context. Queries cover state, version, architecture, version mismatch, post-uninstall absence and advertised-only state. Fixtures are uninstalled after queries; the isolated current-user registry namespace was empty after validation.
- The real MSI and installed .NET inventories exercise bootstrap assess, execute-without-launch and recovery from Running. Existing bootstrap tests cover installer success without satisfaction, reboot and ambiguous recovery.
- Native read-only checks cover installed .NET host inventory, VC++ and Evergreen inventory, and kernel32 file version. Unknown runtime IDs and nonexistent products retain separate outcomes.
- UTF-16 surrogate pairs, changing lengths, bounded allocation, invalid version strings and query refusal use pure adapter tests. These are not claims of native permission-denial or runtime installation coverage.

## Validation and limits

The Windows/bootstrap all-feature suite passed 343 tests. The final prerequisite suite passed 14 tests, the bootstrap suite passed 18 tests, and workspace Clippy passed with warnings denied for Rust code. Workspace doctests, formatting and Linux x86_64 compilation of zup-windows/zup-bootstrap passed.

The elevated MSI machine fixture passed after normal UAC approval. Existing elevated custody tests could not complete: protected acquisition refused a machine payload ancestor that permits untrusted mutation; the interrupted-worker test then failed to reach its barrier. The fail-closed ACL policy was retained. Ordinary custody, reboot, worker protocol and multiprocess tests passed.

No native ARM64 host, x86 process run, domain-managed MSI installation, foreign-user account, real MSI permission-denial scenario or concurrent native uninstall race was available. Uninstall followed by querying is native coverage; racing Win32 return codes and buffer growth are adapter coverage. No real .NET, VC++ or Evergreen components were installed or removed. No power-loss or actual OS reboot was performed.
