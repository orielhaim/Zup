# Automation output

Commands that support machine-readable reporting use `--format`.

| Format | stdout |
| --- | --- |
| `human` | terminal-oriented text |
| `json` | one result document |
| `jsonl` | one protocol event per line |

In machine formats, parse stdout as the protocol stream/document. Logs and other terminal output belong on stderr.

## JSON result

Results use a shared envelope with operation, status, application, targets, artifacts, release/publication data, diagnostics, summary and operation-specific `details`.

Use the checked-in `schema/automation-v1.schema.json` as the exact contract rather than scraping human output.

## JSONL

The stream begins with a version event and ends with `completed`. Intermediate events can describe phases, progress, diagnostics, artifacts, publication and logs.

Consumers should ignore unknown event types within the same protocol major version.

## Stable diagnostic codes

Machine integrations should branch on diagnostic `code`, not on English message text.

## Paths

Protocol paths are release/project-relative and use `/` separators unless a command explicitly reports a local machine path.
