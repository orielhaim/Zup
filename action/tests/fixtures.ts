import type { Inputs } from '../src/inputs.js'
import type { GithubContext, Log } from '../src/ports.js'
import type { AutomationResult } from '../src/protocol.js'

/** A credential that must never appear in an environment a test inspects. */
export const SECRET = 'ghp_this_must_never_appear'

/** What the action was asked to do, with the least that satisfies a test. */
export function inputs(overrides: Partial<Inputs> = {}): Inputs {
  return {
    operation: 'build',
    projectPath: '/w',
    zupVersion: undefined,
    zupPath: undefined,
    releaseDir: 'dist',
    targets: [],
    artifacts: [],
    token: undefined,
    repo: undefined,
    tag: undefined,
    draft: false,
    prerelease: false,
    dryRun: false,
    uploadWorkflowArtifacts: false,
    workflowArtifactName: undefined,
    artifactRetentionDays: undefined,
    attest: false,
    attestPaths: [],
    allowUnsafePublish: false,
    allowUnsigned: false,
    onlineRevocation: false,
    args: [],
    receipt: undefined,
    ...overrides,
  }
}

/** A workflow context whose environment holds both spellings of a credential. */
export function context(overrides: Record<string, string | undefined> = {}): GithubContext {
  const env: Record<string, string | undefined> = {
    // Every one of these is the kind of variable a hosted runner provides, and the
    // action is expected to pass the innocuous ones through while excluding
    // anything credential-shaped.
    RUNNER_OS: 'linux',
    RUNNER_ARCH: 'X64',
    HOME: '/home/runner',
    PATH: '/usr/bin',
    GITHUB_WORKSPACE: '/w',
    GITHUB_OUTPUT: '/w/out',
    GITHUB_ENV: '/w/env',
    GITHUB_PATH: '/w/path',
    GITHUB_STEP_SUMMARY: '/w/summary',
    GITHUB_REPOSITORY: 'acme/acme',
    CI: 'true',
    GITHUB_TOKEN: SECRET,
    GH_TOKEN: SECRET,
    ...overrides,
  }
  return {
    serverUrl: 'https://github.com',
    apiUrl: 'https://api.github.com',
    repository: 'acme/acme',
    runId: '1',
    eventName: 'push',
    event: {},
    env: env as GithubContext['env'],
  }
}

/** A log that discards everything, for a test that asserts on something else. */
export function silentLog(): Log {
  const noop = () => undefined
  return {
    debug: noop,
    info: noop,
    notice: noop,
    warning: noop,
    error: noop,
    startGroup: noop,
    endGroup: noop,
    setSecret: noop,
    summary: noop,
    setOutput: noop,
    annotate: noop,
    fail: noop,
  }
}

/** A log that records everything, so a test can inspect what a user would read. */
export function recordingLog(): { log: Log; lines: string[] } {
  const lines: string[] = []
  const record = (prefix: string) => (message: string) => {
    lines.push(`${prefix} ${message}`)
  }
  return {
    lines,
    log: {
      debug: record('debug'),
      info: record('info'),
      notice: record('notice'),
      warning: record('warning'),
      error: record('error'),
      startGroup: record('group'),
      endGroup: () => undefined,
      setSecret: record('secret'),
      summary: record('summary'),
      setOutput: (name, value) => {
        lines.push(`output ${name}=${value}`)
      },
      annotate: (level, message, location) => {
        lines.push(`annotate ${level} ${message} ${JSON.stringify(location ?? {})}`)
      },
      fail: record('fail'),
    },
  }
}

/**
 * A successful operation result, for a test that only cares about one field.
 *
 * Shaped by hand, unlike the protocol tests: these tests are about the *action's*
 * logic — what it merges, what it renders — and a document zup never emitted would
 * make a failure ambiguous between "the action is wrong" and "the input was wrong".
 * The documents zup really emits live in `protocol-fixtures.ts` and are read from
 * disk there.
 */
export function result(overrides: Partial<AutomationResult> = {}): AutomationResult {
  return {
    protocol: '1.0',
    operation: 'build',
    status: 'success',
    application: { id: 'com.acme.desktop', name: 'Acme', version: '1.4.0' },
    targets: [],
    artifacts: [],
    release_manifest: null,
    publication: null,
    diagnostics: [],
    summary: null,
    details: null,
    ...overrides,
  }
}
