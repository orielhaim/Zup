/**
 * Reading the zup automation protocol.
 *
 * The types come from `protocol.generated.ts`, which `cargo xtask automation
 * generate` derives from the same Rust DTOs the JSON Schema is derived from. This
 * file is the other half: the hand-written decoder that turns those types into
 * values, and it is hand-written on purpose. It is the one place that knows the
 * three rules a consumer has to follow, and a generated decoder would know none of
 * them.
 *
 * ```text
 * 1. Refuse a different major.  Same major: read it.  Different major: stop.
 * 2. Ignore what you do not know.  A field, an event type, an operation name, an
 *    artifact kind or a diagnostic code you have never seen is data, not an error.
 * 3. Refuse a document that says nothing useful.  A failure with no diagnostic, a
 *    success carrying an error, a payload for a different operation: stop, because
 *    the document is not the thing it claims to be.
 * ```
 *
 * # Why there is no first-`{` scan
 *
 * An earlier version searched stdout for the first `{` and parsed from there, on the
 * theory that a stray log line was the common corruption. It is not: zup writes
 * exactly one document to stdout in `--format json` and everything else to stderr,
 * so a `{` in the middle of stdout means stdout is broken, and skipping to it hides
 * the break. The whole of stdout is the document, and this parses all of it.
 */

import type {
  Application,
  Artifact,
  AutomationResult,
  Details,
  Diagnostic,
  Digest,
  Publication,
  Status,
  StreamEvent,
  Target,
} from './protocol.generated.js'

/**
 * The protocol major this action is written against.
 *
 * A constant rather than something read from a document: a document cannot tell the
 * consumer which versions it should have been read by.
 */
export const PROTOCOL_MAJOR = 1

export type {
  Application,
  Artifact,
  ArtifactInspectDetails,
  AutomationResult,
  BuildDetails,
  CheckDetails,
  Details,
  Diagnostic,
  DiagnosticSource,
  Digest,
  DoctorCheck,
  DoctorDetails,
  DoctorTarget,
  LogLevel,
  Publication,
  PublicationAsset,
  Severity,
  SigningEvidence,
  SigningState,
  Status,
  StreamEvent,
  StreamVersion,
  Target,
} from './protocol.generated.js'

import type { SigningEvidence } from './protocol.generated.js'

/** A zup operation, as a name rather than a closed set. */
export type Operation = string

/** Why a document could not be read. */
export class ProtocolError extends Error {
  constructor(
    reason: string,
    readonly raw: string,
  ) {
    super(
      `zup did not emit a readable result: ${reason}. ` +
        'This is a zup bug rather than a configuration problem: the action expects one ' +
        'versioned document on stdout and nothing else.',
    )
    this.name = 'ProtocolError'
  }
}

/** Whether a document's protocol is one this consumer reads. */
export function accepts(protocol: string): boolean {
  return majorOf(protocol) === PROTOCOL_MAJOR
}

function majorOf(protocol: string): number {
  const major = protocol.split('.')[0]
  const value = Number(major)
  return Number.isInteger(value) ? value : Number.NaN
}

/** Whether evidence states that a platform signature covers these bytes. */
export function coversBytes(evidence: readonly SigningEvidence[] | undefined): boolean {
  return evidence?.some((entry) => entry.fact === 'signature_covers_bytes') ?? false
}

/** Whether the artifact carries a finalized, signed identity. */
export function isSigned(artifact: Artifact): boolean {
  return artifact.signing?.state === 'signed'
}

/**
 * Read one `--format json` document.
 *
 * All of `stdout` is the document. A caller that has more than one document on
 * stdout has a zup bug, and saying so is more useful than guessing which one was
 * meant.
 */
export function parseResult(stdout: string, expected?: Operation): AutomationResult {
  const text = stdout.trim()
  if (text.length === 0) {
    throw new ProtocolError('stdout was empty', stdout)
  }
  const value = object(parse(text, stdout), stdout)
  const protocol = string(value['protocol'], 'protocol', stdout)
  if (!accepts(protocol)) {
    throw new ProtocolError(
      `protocol is ${protocol} and this action reads major ${PROTOCOL_MAJOR}`,
      stdout,
    )
  }
  const result = readResult(value, stdout)
  if (expected !== undefined && result.operation !== expected) {
    throw new ProtocolError(
      `expected a \`${expected}\` result and got \`${result.operation}\``,
      stdout,
    )
  }
  const problem = conformance(result)
  if (problem !== undefined) {
    throw new ProtocolError(problem, stdout)
  }
  return result
}

/**
 * Read one line of a `--format jsonl` stream.
 *
 * `undefined` for a line that is not an event, which is a line zup did not write:
 * a blank line, or output from something else. The stream is read line by line for
 * exactly this reason - a line that cannot be read is skipped and the next one is
 * still read, rather than ending the run.
 */
export function parseEvent(line: string): StreamEvent | undefined {
  const text = line.trim()
  if (text.length === 0 || !text.startsWith('{')) {
    return undefined
  }
  let value: unknown
  try {
    value = JSON.parse(text)
  } catch {
    return undefined
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    return undefined
  }
  const record = value as Record<string, unknown>
  const type = record['type']
  if (typeof type !== 'string') {
    return undefined
  }
  switch (type) {
    case 'version':
      return {
        type: 'version',
        protocol: String(record['protocol'] ?? ''),
        zup: String(record['zup'] ?? ''),
        operation: String(record['operation'] ?? ''),
      }
    case 'phase':
      return {
        type: 'phase',
        phase: String(record['phase'] ?? ''),
        message: String(record['message'] ?? ''),
      }
    case 'progress':
      return {
        type: 'progress',
        completed: integer(record['completed'], 0),
        total: integer(record['total'], 0),
        label: String(record['label'] ?? ''),
      }
    case 'diagnostic':
      return { type: 'diagnostic', diagnostic: readDiagnostic(record['diagnostic']) }
    case 'artifact':
      return { type: 'artifact', artifact: readArtifact(record['artifact']) }
    case 'publication':
      return { type: 'publication', publication: readPublication(record['publication']) }
    case 'log':
      return {
        type: 'log',
        level: level(record['level']),
        message: String(record['message'] ?? ''),
      }
    case 'completed':
      return { type: 'completed', result: readResult(asRecord(record['result']), line) }
    default:
      // A message type from a newer zup. Rule 2: keep reading.
      return undefined
  }
}

function level(value: unknown): 'info' | 'warning' | 'error' {
  return value === 'warning' || value === 'error' ? value : 'info'
}

function parse(text: string, raw: string): unknown {
  try {
    return JSON.parse(text)
  } catch (error) {
    // V8's parse error quotes a snippet of the input, so the message is bounded
    // here rather than at the call site: an error message is not the place to paste
    // a build log, and the reader already has the log.
    throw new ProtocolError(
      `${preview((error as Error).message)}; output began: ${preview(text)}`,
      raw,
    )
  }
}

function object(value: unknown, raw: string): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new ProtocolError('the result was not a JSON object', raw)
  }
  return value as Record<string, unknown>
}

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {}
}

function readResult(value: Record<string, unknown>, raw: string): AutomationResult {
  return {
    protocol: String(value['protocol'] ?? ''),
    operation: string(value['operation'], 'operation', raw),
    status: status(value['status']),
    application: readApplication(value['application']),
    targets: list(value['targets']).map(readTarget),
    artifacts: list(value['artifacts']).map(readArtifact),
    release_manifest: optionalString(value['release_manifest']) ?? null,
    publication: readOptionalPublication(value['publication']),
    diagnostics: list(value['diagnostics']).map(readDiagnostic),
    summary: optionalString(value['summary']) ?? null,
    details: readDetails(value['details']),
  }
}

function status(value: unknown): Status {
  // An unreadable status is a failure, not a success: a document that cannot say
  // whether it worked has not worked.
  return value === 'success' ? 'success' : 'failure'
}

function readApplication(value: unknown): Application | null {
  if (value === null || value === undefined) {
    return null
  }
  const record = asRecord(value)
  return {
    id: String(record['id'] ?? ''),
    name: String(record['name'] ?? ''),
    version: String(record['version'] ?? ''),
  }
}

function readTarget(value: unknown): Target {
  const record = asRecord(value)
  return { profile: String(record['profile'] ?? ''), target: String(record['target'] ?? '') }
}

export function readArtifact(value: unknown): Artifact {
  const record = asRecord(value)
  const signing = record['signing']
  return {
    path: String(record['path'] ?? ''),
    digest: readDigest(record['digest']) ?? { algorithm: 'sha256', value: '' },
    size: number(record['size'], 0),
    kind: String(record['kind'] ?? 'unknown'),
    mode: String(record['mode'] ?? 'unknown'),
    id: optionalString(record['id']) ?? null,
    target: optionalString(record['target']) ?? null,
    variants: list(record['variants']).map(String),
    signing:
      signing === null || signing === undefined
        ? null
        : {
            state: String(asRecord(signing)['state'] ?? 'unknown'),
            evidence: list(asRecord(signing)['evidence']).map((entry) => {
              const pair = asRecord(entry)
              return { fact: String(pair['fact'] ?? ''), value: String(pair['value'] ?? '') }
            }),
          },
  }
}

function readOptionalPublication(value: unknown): Publication | null {
  return value === null || value === undefined ? null : readPublication(value)
}

function readPublication(value: unknown): Publication {
  const record = asRecord(value)
  return {
    provider: String(record['provider'] ?? ''),
    subject: String(record['subject'] ?? ''),
    tag: String(record['tag'] ?? ''),
    id: optionalString(record['id']) ?? null,
    state: String(record['state'] ?? 'unknown'),
    url: optionalString(record['url']) ?? null,
    immutable: typeof record['immutable'] === 'boolean' ? record['immutable'] : null,
    assets: list(record['assets']).map((entry) => {
      const asset = asRecord(entry)
      return {
        name: String(asset['name'] ?? ''),
        size: number(asset['size'], 0),
        digest: readDigest(asset['digest']),
        state: String(asset['state'] ?? 'unknown'),
      }
    }),
    receipt: optionalString(record['receipt']) ?? null,
  }
}

function readDigest(value: unknown): Digest | null {
  if (value === null || value === undefined) {
    return null
  }
  const record = asRecord(value)
  return {
    algorithm: String(record['algorithm'] ?? 'sha256'),
    value: String(record['value'] ?? ''),
  }
}

function readDiagnostic(value: unknown): Diagnostic {
  const record = asRecord(value)
  const severity = record['severity']
  const source = record['source']
  return {
    // An unknown level is a notice: a diagnostic zup invented a level for must not
    // fail a release that otherwise succeeded.
    severity: severity === 'error' || severity === 'warning' ? severity : 'notice',
    code: typeof record['code'] === 'string' ? record['code'] : 'zup.internal',
    message: String(record['message'] ?? ''),
    source:
      source === null || source === undefined
        ? null
        : {
            file: String(asRecord(source)['file'] ?? ''),
            start_line: optionalNumber(asRecord(source)['start_line']),
            start_column: optionalNumber(asRecord(source)['start_column']),
            end_line: optionalNumber(asRecord(source)['end_line']),
            end_column: optionalNumber(asRecord(source)['end_column']),
          },
    help: optionalString(record['help']) ?? null,
  }
}

/**
 * The operation's own payload, as the fields this action reads.
 *
 * Not narrowed to the generated thirteen-way union: a consumer that has to
 * discriminate a union before it can look at a field is a consumer that will get
 * the discrimination wrong, and the action reads the keys it knows and ignores the
 * rest. That is rule 2 applied rather than described. The cast is checked from the
 * outside by `conformance`, which requires `details.kind` to name the operation the
 * envelope claims - a payload for a different operation is refused, not narrowed.
 */
function readDetails(value: unknown): Details | null {
  if (value === null || value === undefined) {
    return null
  }
  return value as Details
}

/**
 * Whether a document says something a consumer can act on.
 *
 * The consumer's half of the contract, and deliberately the same three checks zup
 * runs on itself. A document that parses but cannot be used is the failure mode
 * that produces a summary with `-` in it and no error anywhere.
 */
export function conformance(result: AutomationResult): string | undefined {
  if (result.status === 'failure' && result.diagnostics.length === 0) {
    return `\`${result.operation}\` failed without saying why`
  }
  const error = result.diagnostics.find((diagnostic) => diagnostic.severity === 'error')
  if (error !== undefined && result.status === 'success') {
    return `\`${result.operation}\` reports success while carrying the error \`${error.code}\``
  }
  if (result.details !== null && result.details.kind !== result.operation) {
    return `\`${result.operation}\` carries the payload of \`${result.details.kind}\``
  }
  if (result.publication !== null && result.publication.tag.trim().length === 0) {
    return `\`${result.operation}\` published something with no tag`
  }
  return undefined
}

function string(value: unknown, key: string, raw: string): string {
  if (typeof value !== 'string' || value.length === 0) {
    throw new ProtocolError(`the result has no \`${key}\``, raw)
  }
  return value
}

function optionalString(value: unknown): string | undefined {
  return typeof value === 'string' && value.length > 0 ? value : undefined
}

function optionalNumber(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null
}

function number(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback
}

function integer(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isInteger(value) ? value : fallback
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : []
}

/**
 * How much of an unreadable output to quote back.
 *
 * Small on purpose. There are two of these in a parse failure and a fixed suffix,
 * so 80 keeps the whole message under four lines of a workflow log - long enough to
 * see which document broke and where, short enough that a reader reaches the
 * remedy rather than scrolling.
 */
const PREVIEW = 80

function preview(text: string): string {
  const collapsed = text.replaceAll(/\s+/gu, ' ').trim()
  return collapsed.length > PREVIEW ? `${collapsed.slice(0, PREVIEW)}…` : collapsed
}
