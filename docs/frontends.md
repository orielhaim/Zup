# Installer frontends

Zup has three runtime shells over one installer engine:

- `gui` is the normal desktop experience and uses the GPUI frontend.
- `console` is a Windows console application with Cliclack prompts and Indicatif progress.
- `headless` is a non-interactive Windows console application for CI, images, remote sessions, and provisioning.

The shell only presents the shared planner, lifecycle, transaction coordinator, ownership ledger, authenticated worker, updates, plugins, diagnostics, and recovery engine. A package selects one shell at build time.

## Build the runtime templates

```powershell
cargo build -p zup --features build --bin zup --release
cargo build -p zup --no-default-features --features gui --bin zup-setup-gui --release
cargo build -p zup --no-default-features --features console --bin zup-setup-console --release
cargo build -p zup --no-default-features --features headless --bin zup-setup-headless --release
```

`zup-setup` remains a compatibility alias. With the default `gui` feature it is the GUI launcher; the three explicit template names are the supported frontend entry points.

Use one template as the runtime input. The output can still be named `Acme-Setup.exe`:

```powershell
zup build --frontend gui --runtime target\release\zup-setup-gui.exe
zup build --frontend console --runtime target\release\zup-setup-console.exe
zup build --frontend headless --runtime target\release\zup-setup-headless.exe
```

A manifest can set the default:

```toml
frontend = "console"
```

`--frontend` takes precedence. The build rejects a runtime with the wrong PE subsystem or template name. GUI templates use the Windows GUI subsystem. Console and headless templates use the Windows CUI subsystem and are compiled with separate Cargo feature sets.

Check the graph before packaging:

```powershell
cargo tree -p zup --no-default-features --features headless --edges normal
cargo tree -p zup --no-default-features --features console --edges normal
```

The headless graph does not contain `zup-ui`, `gpui-kit`, `cliclack`, `indicatif`, or `console`. The console graph does not contain GPUI. Do not build lightweight templates with unrelated all-feature workspace commands, because Cargo feature unification is global within one invocation. The repository includes `scripts/verify-frontend-features.ps1` to run the graph checks, build all three release templates, verify PE subsystems, and print their sizes.

## Console behavior

A console build uses the normal terminal stream and keeps scrollback. With a real TTY it prompts for scope, optional components, install location, and confirmation. Simple manifests ask only for confirmation. Redirected stdin or stdout, and `--non-interactive`, disable prompts. Cliclack, `console`, and `indicatif` are presentation dependencies only.

Maintenance commands use the same binary and the same shared engine. `modify`, `repair`, `uninstall`, and update retain their normal lifecycle semantics. Ctrl-C requests cooperative cancellation and waits for a transaction boundary. The terminal is restored after success, failure, panic, or cancellation.

## Headless behavior

Headless never prompts, reads stdin, opens a window, or requires a TTY. Use explicit options for automation:

```powershell
.\Acme-Setup.exe install --yes
.\Acme-Setup.exe install --scope machine --component cli --install-dir C:\Apps\Acme --yes
.\Acme-Setup.exe modify --component cli --yes
.\Acme-Setup.exe repair --yes
.\Acme-Setup.exe uninstall --yes
```

`--output human` is readable text, `--output json` is one stable final result, and `--output jsonl` is a versioned event stream. Machine-readable stdout contains only the selected output. Runtime diagnostics and the session log stay on stderr or in the session log.

The event stream uses explicit DTOs, not serialized internal Rust structures:

```json
{"type":"started","protocol_version":1,"application":"Acme","version":"1.4.0","action":"install"}
{"type":"phase","state":"executing"}
{"type":"progress","phase":"files","completed":760,"total":1000,"label":"Installing files"}
{"type":"completed","outcome":"success"}
```

A final JSON result includes `outcome`, `code`, application identity, scope, install directory when known, log path, and drift information. Blocker events include process IDs when Windows Restart Manager supplies them.

## Exit codes

Clap keeps conventional argument and usage behavior. Runtime outcomes use a small stable set:

| Code | Outcome |
| ---: | --- |
| 0 | success |
| 2 | cancelled |
| 3 | invalid invocation or configuration |
| 4 | ownership conflict or drift |
| 5 | elevation required |
| 6 | trust or update verification failure |
| 7 | recovery required |
| 1 | other operation failure |

The same mapping appears in `outcome` for JSON and JSONL callers.

## Elevation and Server Core

Interactive GUI and console sessions may use the existing authenticated UAC worker. A non-interactive operation proceeds when it is already elevated. An unelevated machine-scope operation fails immediately with exit code 5 and does not show a credential or consent dialog.

Run unattended machine installs from an already elevated session:

```powershell
# elevated PowerShell or WinRM/SSH session
.\Acme-Setup.exe install --scope machine --yes
```

The headless and console templates do not link GPUI or graphics/window dependencies. They still use the Win32 APIs required by the Windows backend. Treat Server Core support as a tested compatibility claim: run the Windows multiprocess and security suites on each supported Server Core image before publishing a package for it.

## Apps & Features

GUI packages register GUI Modify and Uninstall commands. Console packages register console maintenance commands. Headless packages set the ARP `NoModify` and `NoRepair` flags and register deterministic, non-interactive uninstall commands. All maintenance files are the selected runtime, so a package never promises a GUI that it did not embed.

## Updates

All frontends use the same TUF client and verified download path:

```powershell
.\Acme-Setup.exe update check
.\Acme-Setup.exe update --yes
```

Headless update output can be consumed with `--output json` or `--output jsonl`; it never asks an unexpected question. The downloaded package is checked for the expected target and frontend before it is started.
