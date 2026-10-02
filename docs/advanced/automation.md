# Machine-readable output

Every operation command takes `--format`. `human` is prose and is not stable; the
other two are a versioned contract.

```bash
zup build --format json    # exactly one result document on stdout
zup build --format jsonl   # one event per line, ending with the same result
```

## Which commands speak it

Operation commands: `check`, `doctor`, `plan`, `build`, `artifact inspect`,
`sign prepare`, `sign verify`, `publish stage`, `publish github`, `toolchain
install`, `toolchain status`, `toolchain clean`.

Not operation commands, because for them the document *is* the product: `schema`
writes a JSON Schema, `completions` writes a shell script. Wrapping either in a
result would be a JSON document inside a JSON document for no reason a consumer
could use. `init`, `preview`, `ui`, `ci` and `fmt` are not in the contract
either.

## The result

```json
{
  "protocol": "1.0",
  "operation": "build",
  "status": "success",
  "application": { "id": "com.acme.desktop", "name": "Acme", "version": "1.4.0" },
  "targets": [{ "profile": "default", "target": "x86_64-pc-windows-msvc" }],
  "artifacts": [
    {
      "path": "Acme-Windows-Setup.exe",
      "digest": { "value": "sha256:1f3a…" },
      "size": 95689241,
      "kind": "universal",
      "mode": "offline",
      "id": "windows",
      "target": null,
      "variants": ["windows-x64"],
      "signing": "pending"
    }
  ],
  "release_manifest": "dist/zup-release.json",
  "publication": null,
  "diagnostics": [],
  "summary": "Acme 1.4.0 · 1 artifact(s)",
  "details": { "kind": "build", "signing_plan": "dist/zup-signing.json", "pending_signatures": 1 }
}
```

`path` is relative to the release root. A build machine's absolute path is not an
identity.

`status` is `success` or `failure`. A failure always carries at least one
diagnostic, so a caller never has to distinguish "failed" from "failed for a
reason nobody wrote down".

## Diagnostics

```json
{
  "severity": "error",
  "code": "zup_manifest::missing_install_directory",
  "message": "install directory for scope `user` is not specified",
  "source": { "file": "zup.toml", "start_line": 21, "start_column": 1 },
  "help": "set [install.directory] `user` and/or `machine` to cover the configured scope"
}
```

`severity` is `error`, `warning` or `notice`. `source` is optional; line and
column are 1-based, and the end position is inclusive.

A code is a lowercase dotted identifier with kebab-case words, and the subject
is the area:

```text
zup.build.plugin_compile_failed
zup.signing.untrusted_chain
zup_manifest::unknown_target_selector
```

Match on the code, not on the message. Messages are written for people and will
be reworded.

## The stream

`--format jsonl` writes one JSON object per line. The first is always `version`,
the last is always `completed`.

| Event | Payload |
| --- | --- |
| `version` | `{ protocol, zup, operation }`. Once, first |
| `phase` | `{ phase, message }` |
| `progress` | `{ completed, total, label }` |
| `diagnostic` | `{ diagnostic }` |
| `artifact` | `{ artifact }` |
| `publication` | `{ publication }` |
| `log` | `{ level, message }` |
| `completed` | `{ result }`. Once, last |

`zup build` emits the phases `validate`, `payload` and `compose`.

Ignore event types you do not know. A field you do not recognise must not fail
the parse.

## Versioning

`protocol` is `major.minor`. Accept the same major at any minor: read what you
understand, ignore the rest. Refuse a different major rather than guessing.

## Refused command lines

If a command line names an operation and a machine format, and the arguments are
wrong, Zup answers with a document rather than a usage message - you asked for a
document, so you get one:

```json
{
  "protocol": "1.0",
  "operation": "build",
  "status": "failure",
  "diagnostics": [{
    "severity": "error",
    "code": "zup.cli.invalid_invocation",
    "message": "error: unexpected argument '--nonsense' found",
    "help": "Run `zup --help`, or `zup <command> --help`, for the arguments this command accepts."
  }]
}
```

A command line that names **no** operation gets a usage message on stderr and a
nonzero exit. A document naming an operation that does not exist would be a worse
answer than the usage text.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Failure. The default for anything unmatched |
| `2` | Cancelled |
| `3` | Invalid invocation, bad configuration, or a usage error |
| `4` | Something else owns a resource the install needs |
| `5` | Elevation or privilege required |
| `6` | Signature, trust, TUF, digest or integrity failure |
| `7` | An interrupted transaction needs reconciling |
| `8` | Another operation holds this installation's lock |
| `3010` | A reboot is required |

`8` is distinct from `1` on purpose: a scheduled retry has to be able to tell
"somebody else is installing this right now" from "this installation is broken".
The first is not a failure of anything.

::: warning The developer CLI's codes are best-effort
A failure that carries typed metadata uses its code. A failure that does not is
classified by reading the error message, so codes `1` through `8` on the
developer CLI are derived rather than guaranteed. Branch on the `status` field
and the diagnostic `code` in `--format json`, not on the exit code. The installed
application's runtime has a typed outcome for every case.
:::

## Schema

```bash
zup schema --output schema/zup.toml.json
```

The authoritative JSON Schema for `zup.toml`, which is what the `#:schema` line
in every generated manifest points at.

The automation contract's schema is published at
`https://zup.orielhaim.com/schema/automation-v1.schema.json`. Nothing in it is closed, and
the envelope's fields are all required.

Next: [the reference](/reference/manifest).
