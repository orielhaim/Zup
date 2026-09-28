# The automation protocol

How a machine reads what zup did. One contract, shared by the developer CLI, CI
and the GitHub Action, specified in `crates/zup-automation` and generated into
four artifacts:

```text
crates/zup-automation/            the contract, in Rust
schema/automation-v1.schema.json  the contract, for a consumer that is not TypeScript
action/src/protocol.generated.ts  the contract, as TypeScript declarations
fixtures/automation/*              golden documents both sides are tested against
```

```bash
cargo xtask automation generate   # write the artifacts
cargo xtask automation check      # fail if they are stale
```

A Rust DTO that changed without its artifacts being regenerated fails `check`.
That is the point: the Action cannot end up compiled against a shape zup stopped
emitting.

## The three formats

| `--format` | stdout |
| --- | --- |
| `human` | prose. Never parsed, and free to change. |
| `json` | exactly one `AutomationResult`, and nothing else |
| `jsonl` | the protocol stream: one `StreamEvent` per line |

Everything else a command wants to say goes to **stderr** in both machine
formats. That is not tidiness: a `--format json` consumer is reading stdout, and a
log line in the middle of its document is a parse failure in a pipeline that
spent ten minutes building. The rule is stated once in `crates/zup/src/report.rs`,
enforced by the single `Reporter` every command reports through, and checked by
`crates/zup/tests/protocol_stdout.rs` - including the adversarial case, where a
child process on `PATH` writes a kilobyte of junk to stdout.

`schema` and `completions` do **not** take `--format`. Their product *is* a
document, and wrapping either would be a JSON document inside a JSON document for
no reason a consumer could use. The classification is made once in
`crates/zup/src/cli.rs`, so a command cannot grow the flag by accident.

## The result envelope

One shape for every operation. Fields an integration needs are in the envelope;
everything specific to one command is in `details`, tagged with the operation's
own name.

```json
{
  "protocol": "1.0",
  "operation": "build",
  "status": "success",
  "application": { "id": "com.acme.desktop", "name": "Acme", "version": "1.4.0" },
  "targets": [{ "profile": "windows-x64", "target": "x86_64-pc-windows-msvc" }],
  "artifacts": [
    { "path": "Acme-Windows-Setup.exe", "digest": { "algorithm": "sha256", "value": "3b1f…" },
      "size": 248512896, "kind": "single", "mode": "offline", "id": "windows-x64" }
  ],
  "release_manifest": "dist/zup-release.json",
  "publication": null,
  "diagnostics": [],
  "summary": "Built 1 artifact for 1 target",
  "details": { "kind": "build", "signing_plan": "dist/zup-signing.json" }
}
```

Every field is always written, `null` where a command has nothing to say, so a
consumer gets `null` rather than having to distinguish "not applicable" from
"produced by a zup older than this contract".

A consumer that has to learn a different shape per command has to learn N shapes
and still handle the Nth+1 somebody adds. The alternative - one struct with
`check_result`, `build_result`, `publish_result`, … all nullable - is worse: a
consumer cannot tell "this field does not apply" from "this field is missing
because the producer is older".

`details.kind` is always the same string as `operation`, so there is one vocabulary
rather than two to keep in step. `AutomationResult::validate` refuses a result
whose payload names a different operation, and so does the Action's decoder. A
payload that does not match its envelope is a document whose two halves are about
different things.

## Diagnostics

```json
{
  "severity": "error",
  "code": "zup.manifest.unknown_target",
  "message": "resource references unknown target profile `x64`",
  "source": { "file": "zup.toml", "start_line": 12, "start_column": 3 },
  "help": "use an exact profile id declared under [build.targets]"
}
```

**The code is the part an integration can act on.** It is chosen at the call site
where somebody decided what the failure *means*. A Rust type name is not a code:
`zup_manifest::UnknownTarget` is renamed the day somebody reorganises a module, and
every integration that matched on it would break on a change no operator could see.

`miette` produces the message and span; `zup-automation` does not depend on it,
and `crates/zup/src/failure.rs` is the adapter. An error carrying none of the four
promised fields still becomes a real diagnostic under `zup.internal` - a code a
consumer cannot match on is better than silence.

### Severity and status agree

An `error` diagnostic on a `success` result is a document a consumer cannot use:
one that treats it as success ships a broken release, one that treats it as failure
has no exit code to back it up. `validate` refuses it, and so does the Action.

This is why a project that is valid but cannot produce one universal installer
carries a **warning**: `zup check` exits zero, and a consumer that failed on that
diagnostic would be wrong.

## The stream

`--format jsonl` writes one document per line:

| `type` | carries |
| --- | --- |
| `version` | the protocol, the zup release, the operation about to run |
| `phase` | a step that began |
| `progress` | a counter, with a total |
| `diagnostic` | a diagnostic, as it was found |
| `artifact` | a file that was produced |
| `publication` | a release that went somewhere |
| `log` | human output that would have gone to a terminal |
| `completed` | the result, and the end of the stream |

Three rules make it readable rather than merely parseable.

**The version line is first.** A consumer that has to look past progress events to
find out whether it can read the stream has no way to decide whether to keep
reading.

**The completed line is last, and carries the same document `--format json` would
have written.** A consumer that reported progress from the events and a summary
from the result must not be able to disagree with itself.

**An unknown event type is skipped, not fatal.** `StreamEvent` has a decode-only
`#[serde(other)]` arm for exactly this, and the Action's decoder returns
`undefined` and keeps reading.

## Versioning

`MAJOR.MINOR`, and the rule is the ordinary one:

- **same major, any minor** - read it, ignoring fields, event types, operation
  names, artifact kinds, publication states, and diagnostic codes you do not
  recognise;
- **different major** - refuse, and say which versions were involved.

A minor may add a field, event type, operation, or diagnostic code. It may not
remove or repurpose one. `protocol.accepts` is the whole implementation.

## Open vocabularies

`Identifier` is the one wire type for every name in the protocol - operation
names, artifact kinds and modes, publication states, diagnostic codes, check
kinds.

```text
segment  ::= [a-z] ( [a-z0-9] | separator )*
name     ::= segment ('.' segment)*
```

A segment starts with a lowercase letter, ends with a letter or digit, and joins
words with `_` or `-`, never two separators in a row. So `build`,
`publish.github`, `zup.manifest.unknown_target` and `single-target` are
identifiers; `Build`, `publish..github`, `zup/manifest` and `unknown__target` are
not.

The rule applies **in both directions**: a producer cannot emit an ungrammatical
name, and a consumer cannot be handed one. That is what stops an unbounded string
reaching a consumer's matching. Enumerating these values in Rust would make every
extension a wire break, which is backwards - the vocabulary is supposed to grow.
`ALL_OPERATIONS` exists for the checks about the *set*, not as a closed enum.

## Numbers

`ByteCount` is a newtype bounded at `2^53 - 1`, the largest integer a JavaScript
`number` holds exactly, and every non-Rust consumer is one. **Producers saturate**
rather than fail, because refusing to report a 9 petabyte artifact would be
reporting a problem about zup's own types. **Consumers refuse** on decode, because
a silently-rounded size is a wrong answer rather than an approximate one.

`Publication.id` is a string for the same reason: it is the provider's own
reference and whether it fits in a double is the provider's business.

## Paths

Every path in a result is **relative and `/`-separated**: an artifact `path`
relative to the release root, `release_manifest` and `publication.receipt`
relative to the project, a diagnostic's `source.file` relative to the project. An
absolute build-machine path is not an identity - the same artifact built on a
different agent lives elsewhere.

The one exception is `toolchain.status`'s `cache` field, which answers "where
would a build on *this* machine look" - the question the command exists to answer.

## Reading it from TypeScript

The generated declarations are the types. The decoder in `action/src/protocol.ts`
is hand-written on purpose: it is where the ignore-what-you-do-not-know rule
lives, and a generated decoder would know none of it.

```ts
import { parseResult, parseEvent, ProtocolError } from './protocol.js'

const result = parseResult(stdout, 'build')
for (const artifact of result.artifacts) {
  console.log(artifact.path, artifact.digest.value)
}
```

Two rules it implements that a parser alone would not:

- **A whole stream, not a search for a `{`.** An earlier version searched stdout
  for the first brace, on the theory that a stray log line was the common
  corruption. It is not: zup writes one document to stdout and everything else to
  stderr, so a `{` in the middle of stdout *means stdout is broken*, and skipping
  to it hides the break.
- **A `Status` it cannot read is a failure**, not a success. A document that cannot
  say whether it worked has not worked.

`LineFramer` in `action/src/stream.ts` exists rather than a `split('\n')` on the
finished output, for two reasons a pipe forces: a chunk boundary can fall in the
middle of a UTF-8 sequence (and per-chunk `toString('utf8')` turns that into
`U+FFFD`, so a document naming a file with an emoji in it stops parsing), and a
line has to be delivered before the process exits or the stream is just a log.

## The fixtures

`fixtures/automation/` holds golden documents **serialized by the same Rust code
that writes a real result**, not transcribed. A hand-written fixture in TypeScript
proves a TypeScript object satisfies a TypeScript interface, which is true of every
fixture ever written and says nothing about whether Rust serializes that shape.
These do: a changed field name moves the fixture, the Action's decoder fails, and
the failure names the field.

Nothing in a fixture is derived from the environment - no timestamps, no absolute
paths, no digests of files that exist only on the machine that wrote them - so the
same commit produces the same bytes on every host and `check` is a diff. The
Action's tests read these files from disk rather than keeping a copy, which is what
makes `bun test` a compatibility gate.

## The two protocols

Two machine contracts live in this repository and are not interchangeable.

| | `zup-automation` | `zup-presentation` |
| --- | --- | --- |
| speaks for | the developer CLI | an installed application |
| produced by | `zup build`, `zup publish`, … | the installer a user runs |
| versioned by | `PROTOCOL` (`1.0`) | `INSTALLER_PROTOCOL_VERSION` |
| read by | CI, the GitHub Action | the update client |

They change for different reasons: `zup-automation` when the developer CLI gains
an operation, `zup-presentation` when the installer gains a lifecycle state. A
client that has installed 1.4.0 must not be broken by a decision made in 1.5.0
about what `zup publish` reports.
