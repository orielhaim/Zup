/**
 * The action's inputs.
 *
 * The criterion for what belongs here is *execution*, not *application semantics*.
 * Everything that describes what an application is — its name, its installer
 * metadata, its update channels, which repository it publishes to — lives in
 * `zup.toml`, versioned with the project and reviewable next to the build that
 * consumes it. An input that duplicates a manifest field is a second source of truth
 * that can disagree with the first.
 *
 * What is here is what a workflow must decide and `zup.toml` cannot know: which
 * operation this step is, where the project is, which zup to install, and the one
 * credential.
 *
 * `RAW` is the single place an input name is spelled, so the set is enumerable —
 * which is what `action/scripts/check-metadata.mjs` needs to prove `action.yml` and
 * this file agree — and renaming an input is one edit rather than a search.
 */

import * as core from '@actions/core'

/** What this step does. */
export type Operation =
  | 'setup'
  | 'build'
  | 'compose'
  | 'finalize'
  | 'attest'
  | 'publish'
  | 'release'

/** Everything the action was asked to do. */
export interface Inputs {
  operation: Operation
  /** Absolute path to the directory holding `zup.toml`. */
  projectPath: string
  /** The zup version to install, or `undefined` for this build's tested default. */
  zupVersion: string | undefined
  /** An explicit zup executable, which skips installation entirely. */
  zupPath: string | undefined
  /** The release directory, relative to the project. */
  releaseDir: string
  /** Target profiles or triples to build. */
  targets: string[]
  /** Declared distribution artifacts to compose. */
  artifacts: string[]
  /** The publish credential, absent unless the step was given one. */
  token: string | undefined
  /** Override the repository a release belongs to, as `owner/name`. */
  repo: string | undefined
  tag: string | undefined
  draft: boolean
  prerelease: boolean
  dryRun: boolean
  uploadWorkflowArtifacts: boolean
  /** The workflow artifact name, or `undefined` to derive one. */
  workflowArtifactName: string | undefined
  artifactRetentionDays: number | undefined
  attest: boolean
  /** Whether a release with no signature may still be finalized and published. */
  allowUnsigned: boolean
  /** Whether revocation may be checked over the network. */
  onlineRevocation: boolean
  /** Extra subjects to attest, relative to the release directory. */
  attestPaths: string[]
  /** Whether a dangerous trigger may proceed with a credential. */
  allowUnsafePublish: boolean
  /** Extra arguments, already tokenized. */
  args: string[]
  /** Where the receipt is written, relative to the project. */
  receipt: string | undefined
}

/** A malformed input, with the name and what was expected. */
export class InputError extends Error {
  constructor(
    readonly input: string,
    reason: string,
    readonly remedy: string,
  ) {
    super(`\`${input}\` ${reason}. ${remedy}`)
    this.name = 'InputError'
  }
}

const OPERATIONS: readonly Operation[] = [
  'setup',
  'build',
  'compose',
  'finalize',
  'attest',
  'publish',
  'release',
]

const RAW: Readonly<Record<string, string>> = Object.freeze(
  Object.fromEntries(
    [
      'operation',
      'project-path',
      'zup-version',
      'zup-path',
      'release-dir',
      'target',
      'artifact',
      'github-token',
      'repo',
      'tag',
      'draft',
      'prerelease',
      'dry-run',
      'upload-workflow-artifacts',
      'workflow-artifact-name',
      'artifact-retention-days',
      'attest',
      'attest-paths',
      'allow-unsafe-publish',
      'receipt',
      'args',
    ].map((name) => [name, core.getInput(name)]),
  ),
)

/** Read every input, refusing anything ambiguous. */
export function readInputs(): Inputs {
  return {
    operation: readOperation(),
    projectPath: core.toPlatformPath(text('project-path') ?? '.'),
    zupVersion: text('zup-version'),
    zupPath: text('zup-path'),
    releaseDir: text('release-dir') ?? 'dist',
    targets: list('target'),
    artifacts: list('artifact'),
    token: readToken(),
    repo: text('repo'),
    tag: text('tag'),
    draft: flag('draft'),
    prerelease: flag('prerelease'),
    dryRun: flag('dry-run'),
    uploadWorkflowArtifacts: flag('upload-workflow-artifacts'),
    workflowArtifactName: text('workflow-artifact-name'),
    artifactRetentionDays: number('artifact-retention-days'),
    attest: flag('attest'),
    allowUnsigned: flag('allow-unsigned'),
    onlineRevocation: flag('online-revocation'),
    attestPaths: list('attest-paths'),
    allowUnsafePublish: flag('allow-unsafe-publish'),
    args: tokenize(text('args') ?? '', process.platform),
    receipt: text('receipt'),
  }
}

function readOperation(): Operation {
  const raw = text('operation') ?? 'build'
  const found = OPERATIONS.find((candidate) => candidate === raw)
  if (!found) {
    throw new InputError(
      'operation',
      `is \`${raw}\`, which is not an operation`,
      `Expected one of: ${OPERATIONS.join(', ')}.`,
    )
  }
  return found
}

/**
 * The publish credential.
 *
 * A missing token is not an error until an operation that needs one runs, so a build
 * with no token at all works. A workflow that copies an expression into a place it
 * is not interpolated leaves the literal text behind, which can only ever fail
 * later as a confusing 401; it is treated as no token at all.
 */
function readToken(): string | undefined {
  const raw = RAW['github-token'] ?? ''
  if (raw.length === 0 || raw.includes(UNINTERPOLATED)) {
    return undefined
  }
  return raw
}

/** The opening of a workflow expression the runner failed to interpolate. */
const UNINTERPOLATED = '${{'

/** An input's value, or `undefined` when it was left empty. */
function text(name: string): string | undefined {
  const value = RAW[name] ?? ''
  return value.length > 0 ? value : undefined
}

/**
 * A repeatable input, written as a multi-line or a comma-separated list.
 *
 * Both, because a YAML block scalar is the readable form for a matrix and a comma
 * list is the readable form for two values, and a developer should not have to know
 * which this action prefers.
 */
function list(name: string): string[] {
  const raw = text(name)
  if (raw === undefined) {
    return []
  }
  return raw
    .split(/[\n,]/u)
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0)
}

function flag(name: string): boolean {
  const raw = text(name)
  if (raw === undefined) {
    return false
  }
  const normalized = raw.trim().toLowerCase()
  if (normalized === 'true' || normalized === '1' || normalized === 'yes') {
    return true
  }
  if (normalized === 'false' || normalized === '0' || normalized === 'no') {
    return false
  }
  throw new InputError(name, `is \`${raw}\`, which is not a boolean`, 'Expected `true` or `false`.')
}

function number(name: string): number | undefined {
  const raw = text(name)
  if (raw === undefined) {
    return undefined
  }
  const value = Number(raw)
  if (!Number.isInteger(value) || value < 1) {
    throw new InputError(
      name,
      `is \`${raw}\`, which is not a whole number of days`,
      'Expected a number of at least 1.',
    )
  }
  return value
}

/**
 * Split an advanced-arguments input into argv.
 *
 * Never a shell. The action spawns zup with an argument vector, so a value
 * containing `;`, `|`, `&&` or a backtick is just a string — there is no interpreter
 * to give it meaning. That is the whole security property, and it is why this
 * function is allowed to be small.
 *
 * The two quoting rules are genuinely different. POSIX: single quotes are literal,
 * double quotes allow `\"`, and a backslash escapes the next character outside single
 * quotes. Windows: quotes group, and a backslash is a *path separator*, not an
 * escape — `C:\Program Files\zup\zup.exe` is one argument containing no escapes at
 * all, and treating `\` as an escape there would silently delete it.
 */
export function tokenize(input: string, platform: NodeJS.Platform = process.platform): string[] {
  const windows = platform === 'win32'
  const args: string[] = []
  let current = ''
  /** The quote character currently open, or `undefined`. */
  let open: '"' | "'" | undefined
  /** Whether this argument has begun, which an empty `""` does. */
  let started = false

  for (let index = 0; index < input.length; index += 1) {
    const character = input[index] as string

    // A backslash is an escape on POSIX and data on Windows, except where it
    // precedes a quote — the one case CommandLineToArgvW also treats as an escape,
    // which is what makes a path containing `\"` survive.
    if (character === '\\') {
      const next = input[index + 1]
      const escaping = !windows && open !== "'"
      if (escaping) {
        if (next === undefined) {
          throw new InputError('args', 'ends with `\\`', 'Quote the value instead.')
        }
        current += next
        index += 1
        started = true
        continue
      }
      if (windows && open === undefined && next === '"') {
        current += '"'
        index += 1
        started = true
        continue
      }
      current += '\\'
      started = true
      continue
    }

    if (character === '"' || character === "'") {
      // On Windows a single quote is not special to the C runtime at all, so
      // treating it as one would break `don't`. Elsewhere a quote of the *other*
      // kind inside an open run is literal: `'say "hi"'` is one argument.
      if (windows && character === "'") {
        current += character
        started = true
        continue
      }
      if (open === character) {
        open = undefined
        started = true
        continue
      }
      if (open === undefined) {
        open = character
        started = true
        continue
      }
      current += character
      started = true
      continue
    }

    if (open !== undefined || !/\s/u.test(character)) {
      current += character
      started = true
      continue
    }

    if (started) {
      args.push(current)
      current = ''
      started = false
    }
  }

  if (open !== undefined) {
    throw new InputError('args', 'has an unclosed quote', 'Close it, or drop the argument.')
  }
  if (started) {
    args.push(current)
  }
  return args
}
