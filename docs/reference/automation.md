# Automation output

Commands that support machine-readable reporting use `--format`.

| Format | stdout |
| --- | --- |
| `human` | terminal-oriented text, not stable |
| `json` | one result document |
| `jsonl` | one protocol event per line |

In machine formats, parse stdout as the protocol stream/document. Logs and other terminal output belong on stderr.

Operation commands: `check`, `doctor`, `plan`, `build`, `artifact inspect`, `sign prepare`, `sign verify`, `publish stage`, `publish github`, `toolchain install`, `toolchain status`, `toolchain clean`.

## JSON result

Results use a shared envelope with operation, status, application, targets, artifacts, release/publication data, diagnostics, summary and operation-specific `details`. `path` values are release/project-relative. A failure always carries at least one diagnostic.

Use the checked-in `schema/automation-v1.schema.json` as the exact contract rather than scraping human output.

## JSONL

The stream begins with a `version` event and ends with `completed`.

| Event | Payload |
| --- | --- |
| `version` | `{ protocol, zup, operation }` |
| `phase`, `progress` | `{ phase, message }`, `{ completed, total, label }` |
| `diagnostic`, `artifact`, `publication`, `log` | the named object |
| `completed` | `{ result }` |

Ignore event types and fields you do not recognise within the same protocol major version.

## Stable diagnostic codes

Machine integrations should branch on diagnostic `code`, not on English message text. A code is a lowercase dotted identifier (`zup.build.plugin_compile_failed`); messages are written for people and will be reworded.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | success |
| `1` | failure |
| `2` | cancelled |
| `3` | invalid invocation or configuration |
| `4` | another owner holds a needed resource |
| `5` | elevation required |
| `6` | signature, trust, or integrity failure |
| `7` | an interrupted transaction needs reconciling |
| `8` | another operation holds the installation lock |
| `3010` | reboot required |

Branch on the `status` field and diagnostic `code` in `--format json`, not on the exit code: failures without typed metadata are classified from their message, so CLI codes are best-effort. The installed runtime reports a typed outcome for every case.

## Generated Console and Headless installers

Generated installers use `--output`, independently of the developer CLI's `--format` protocol:

```sh
./Acme-Setup install --scope user --non-interactive --output json
./Acme-Setup repair --scope user --output jsonl
```

`json` writes one `InstallerResult` document. It includes `protocol_version`, `outcome`, `code`, application/version, scope, install directory, log path, message, and preserved repair drift. `jsonl` begins with `started`, streams `phase`, `progress`, prerequisite and cancellation events, then ends with exactly one `completed` or `failed` event. Failed events carry the typed outcome, numeric code, message, and diagnostic. JSON modes never prompt. Stdout contains only this protocol; diagnostics belong on stderr.

Branch on `outcome` and `code`. A committed transaction with incomplete reconciliation reports `recovery_required`, not success. Preserve transaction state and the installer when recovery is required. The reported state directory, transaction identifiers when inspectable, and log path help locate the interrupted operation. Failure to inspect state remains an error.

Windows preserves the native reboot exit code `3010`; on Unix, where process exit codes are limited to eight bits, use the full numeric code and outcome in the structured result. Windows installed self-uninstall entries hand off to a helper; invoke the downloaded installer when a supervising process needs the final exit status.

## Paths

Protocol paths are release/project-relative and use `/` separators unless a command explicitly reports a local machine path.
