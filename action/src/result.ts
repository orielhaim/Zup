/**
 * The shape zup emits for machines, and the shape this action reads.
 *
 * The action runs zup with `--format json` and parses one versioned envelope whose
 * `schema` field is the contract. A zup that changes the envelope bumps `schema`, and
 * this file is where a new version is taught.
 *
 * The envelope is deliberately not action-specific. The same document is what a Tauri
 * adapter or a CI system that is not GitHub would read, and a contract only the action
 * can produce gets extended one field at a time forever.
 */

/** The envelope version this action understands. */
export const RESULT_SCHEMA = 1

/** One zup operation. */
export type Operation = 'setup' | 'build' | 'compose' | 'attest' | 'publish' | 'release'

/** A diagnostic severity, in the three levels the runner can render. */
export type DiagnosticSeverity = 'error' | 'warning' | 'notice'

/** Where a diagnostic points. */
export interface DiagnosticSource {
  file: string
  startLine?: number | undefined
  startColumn?: number | undefined
  endLine?: number | undefined
  endColumn?: number | undefined
}

/** One thing zup wants a developer to know. */
export interface Diagnostic {
  severity: DiagnosticSeverity
  /** A stable identifier, e.g. `zup_manifest::unknown_target_profile_reference`. */
  code: string
  message: string
  source?: DiagnosticSource | undefined
  /** A line the developer can act on. */
  help?: string | undefined
}

/** One file a build produced. */
export interface ArtifactResult {
  /** The release-relative path, which is what a consumer downloads. */
  path: string
  /** Lowercase hex, 64 characters. */
  digest: string
  size: number
  kind: string
  mode: string
  /** Signing state, when the build got that far. */
  signature?: string | undefined
}

/** The release a publication created or found. */
export interface ReleaseResult {
  repository: string
  host: string
  tag: string
  releaseId: number
  state: string
  url?: string | undefined
  immutable?: boolean | undefined
  assets: { name: string; size: number; digest: string; state: string }[]
}

/**
 * The machine-readable result of one zup operation.
 *
 * Optional fields are `| undefined` rather than merely optional because
 * `exactOptionalPropertyTypes` is on: a field set to `undefined` and a field absent
 * mean the same thing here, which is what the wire format says too.
 */
export interface OperationResult {
  schema: number
  operation: Operation | string
  success: boolean
  appVersion?: string | undefined
  targets: string[]
  artifacts: ArtifactResult[]
  releaseManifest?: string | undefined
  release?: ReleaseResult | undefined
  diagnostics: Diagnostic[]
  /** A one-line human summary zup already produced. */
  summary?: string | undefined
}

/** Why a result could not be read. */
export class ResultFormatError extends Error {
  constructor(
    reason: string,
    readonly raw: string,
  ) {
    super(
      `zup did not emit a readable result: ${reason}. This is a zup bug rather than a ` +
        'configuration problem — the action expects one versioned JSON envelope on stdout.',
    )
    this.name = 'ResultFormatError'
  }
}

/** Parse zup's machine result. */
export function parseResult(stdout: string, expected: Operation): OperationResult {
  const text = stdout.trim()
  if (text.length === 0) {
    throw new ResultFormatError('stdout was empty', stdout)
  }
  // A log line before the document is the common corruption, so the search is for
  // an object rather than for the whole stream.
  const start = text.indexOf('{')
  if (start === -1) {
    throw new ResultFormatError(`no JSON object in stdout: ${preview(text)}`, stdout)
  }
  const candidate = text.slice(start)
  let parsed: unknown
  try {
    parsed = JSON.parse(candidate)
  } catch (error) {
    throw new ResultFormatError(
      `${(error as Error).message}; output began: ${preview(candidate)}`,
      stdout,
    )
  }
  if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) {
    throw new ResultFormatError('the result was not a JSON object', stdout)
  }
  const value = parsed as Record<string, unknown>
  const schema = value['schema']
  if (typeof schema !== 'number') {
    throw new ResultFormatError('the result has no numeric `schema`', stdout)
  }
  if (schema !== RESULT_SCHEMA) {
    throw new ResultFormatError(
      `schema is ${schema} and this action understands ${RESULT_SCHEMA}`,
      stdout,
    )
  }
  const operation = value['operation']
  if (typeof operation !== 'string') {
    throw new ResultFormatError('the result has no `operation`', stdout)
  }
  if (operation !== expected) {
    throw new ResultFormatError(
      `expected a \`${expected}\` result and got \`${operation}\``,
      stdout,
    )
  }
  return {
    schema,
    operation,
    success: value['success'] === true,
    appVersion: asString(value, 'appVersion'),
    targets: asStrings(value, 'targets'),
    artifacts: asArray(value, 'artifacts').flatMap(parseArtifact),
    releaseManifest: asString(value, 'releaseManifest'),
    release: parseRelease(value['release']),
    diagnostics: asArray(value, 'diagnostics').flatMap(parseDiagnostic),
    summary: asString(value, 'summary'),
  }
}

function asString(value: Record<string, unknown>, key: string): string | undefined {
  const found = value[key]
  return typeof found === 'string' && found.length > 0 ? found : undefined
}

function asArray(value: Record<string, unknown>, key: string): unknown[] {
  const found = value[key]
  return Array.isArray(found) ? found : []
}

/**
 * A list of strings, dropping non-string entries rather than coercing them: a
 * target named `1` and a target named `"1"` are the same to a shell and different
 * to a workflow expression, and guessing which zup meant is how a matrix silently
 * builds the wrong target.
 */
function asStrings(value: Record<string, unknown>, key: string): string[] {
  return asArray(value, key).filter((entry): entry is string => typeof entry === 'string')
}

function parseArtifact(found: unknown): ArtifactResult[] {
  if (typeof found !== 'object' || found === null) {
    return []
  }
  const value = found as Record<string, unknown>
  const path = value['path']
  const digest = value['digest']
  if (typeof path !== 'string' || typeof digest !== 'string') {
    return []
  }
  return [
    {
      path,
      digest,
      size: typeof value['size'] === 'number' ? value['size'] : 0,
      kind: typeof value['kind'] === 'string' ? value['kind'] : 'unknown',
      mode: typeof value['mode'] === 'string' ? value['mode'] : 'unknown',
      signature: asString(value, 'signature'),
    },
  ]
}

function parseDiagnostic(found: unknown): Diagnostic[] {
  if (typeof found !== 'object' || found === null) {
    return []
  }
  const value = found as Record<string, unknown>
  const message = value['message']
  if (typeof message !== 'string') {
    return []
  }
  const severity = value['severity']
  return [
    {
      // Unknown severity is a notice: a diagnostic zup invented a level for must
      // not fail a release that otherwise succeeded.
      severity: severity === 'error' || severity === 'warning' ? severity : 'notice',
      code: typeof value['code'] === 'string' ? value['code'] : 'zup::unknown',
      message,
      source: parseSource(value['source']),
      help: asString(value, 'help'),
    },
  ]
}

function parseSource(found: unknown): DiagnosticSource | undefined {
  if (typeof found !== 'object' || found === null) {
    return undefined
  }
  const value = found as Record<string, unknown>
  const file = value['file']
  if (typeof file !== 'string' || file.length === 0) {
    return undefined
  }
  return {
    file,
    startLine: asNumber(value, 'startLine'),
    startColumn: asNumber(value, 'startColumn'),
    endLine: asNumber(value, 'endLine'),
    endColumn: asNumber(value, 'endColumn'),
  }
}

function parseRelease(found: unknown): ReleaseResult | undefined {
  if (typeof found !== 'object' || found === null) {
    return undefined
  }
  const value = found as Record<string, unknown>
  const tag = value['tag']
  if (typeof tag !== 'string') {
    return undefined
  }
  return {
    repository: typeof value['repository'] === 'string' ? value['repository'] : '',
    host: typeof value['host'] === 'string' ? value['host'] : '',
    tag,
    releaseId: typeof value['releaseId'] === 'number' ? value['releaseId'] : 0,
    state: typeof value['state'] === 'string' ? value['state'] : 'unknown',
    url: asString(value, 'url'),
    immutable: typeof value['immutable'] === 'boolean' ? value['immutable'] : undefined,
    assets: asArray(value, 'assets').flatMap((asset) => {
      if (typeof asset !== 'object' || asset === null) {
        return []
      }
      const record = asset as Record<string, unknown>
      const name = record['name']
      if (typeof name !== 'string') {
        return []
      }
      return [
        {
          name,
          size: typeof record['size'] === 'number' ? record['size'] : 0,
          digest: typeof record['digest'] === 'string' ? record['digest'] : '',
          state: typeof record['state'] === 'string' ? record['state'] : 'unknown',
        },
      ]
    }),
  }
}

function asNumber(value: Record<string, unknown>, key: string): number | undefined {
  const found = value[key]
  return typeof found === 'number' && Number.isFinite(found) ? found : undefined
}

/** How much of an unreadable output to quote back. */
const PREVIEW = 120

function preview(text: string): string {
  const collapsed = text.replaceAll(/\s+/gu, ' ').trim()
  return collapsed.length > PREVIEW ? `${collapsed.slice(0, PREVIEW)}…` : collapsed
}
