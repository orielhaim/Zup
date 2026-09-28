import { describe, expect, it } from 'bun:test'
import type { SourceLocation } from '../src/ports.js'
import { redact } from '../src/runtime.js'
import { annotate, buildEnvironment, INHERITED, TOKEN_VARIABLES } from '../src/workflow.js'
import { context, inputs, recordingLog, result, SECRET } from './fixtures.js'

/**
 * The secret-isolation acceptance criterion, as executable assertions.
 *
 * Each of these is a hard requirement, not a nicety: `zup build` may invoke Tauri,
 * Electron, Cargo build scripts and npm scripts, and a token in that environment is a
 * token handed to whatever the project's build does.
 */
describe('secret isolation', () => {
  it('gives a build subprocess no token at all', () => {
    const env = buildEnvironment('build', inputs({ token: SECRET }), context())
    for (const name of TOKEN_VARIABLES) {
      expect(env[name]).toBeUndefined()
    }
    expect(JSON.stringify(env)).not.toContain(SECRET)
  })

  it('gives a compose subprocess no token at all', () => {
    expect(
      JSON.stringify(buildEnvironment('compose', inputs({ token: SECRET }), context())),
    ).not.toContain(SECRET)
  })

  it('gives an attest subprocess no token at all', () => {
    expect(
      JSON.stringify(buildEnvironment('attest', inputs({ token: SECRET }), context())),
    ).not.toContain(SECRET)
  })

  it('gives the publish subprocess the token, under both variable names', () => {
    // zup reads `GH_TOKEN` then `GITHUB_TOKEN`. Both are set so a developer's local
    // `gh auth` cannot shadow a job-scoped token with a stale one.
    const env = buildEnvironment('publish', inputs({ token: SECRET }), context())
    expect(env['GH_TOKEN']).toBe(SECRET)
    expect(env['GITHUB_TOKEN']).toBe(SECRET)
  })

  it('gives the publish subprocess nothing when no token was provided', () => {
    const env = buildEnvironment('publish', inputs(), context({ GITHUB_TOKEN: SECRET }))
    expect(env['GH_TOKEN']).toBeUndefined()
    expect(env['GITHUB_TOKEN']).toBeUndefined()
  })

  it('never forwards the parent PATH, which is not in the allowlist', () => {
    // A PATH a developer set by hand is a PATH the action did not choose. zup still
    // gets a working one from the runner's own variables.
    const env = buildEnvironment('build', inputs(), context({ PATH: '/poisoned' }))
    expect(Object.hasOwn(env, 'PATH')).toBe(false)
  })

  it('passes through the variables a build genuinely needs', () => {
    const env = buildEnvironment('build', inputs(), context())
    for (const name of ['RUNNER_OS', 'RUNNER_ARCH', 'HOME', 'GITHUB_WORKSPACE', 'CI']) {
      expect(env[name]).toBeDefined()
    }
  })

  it('builds every environment from an allowlist, not from the parent', () => {
    // The structural assertion: a variable nobody thought about cannot reach a
    // build, because the build's environment is a fresh object populated from
    // `INHERITED` alone.
    const allowed: readonly string[] = [...INHERITED, ...TOKEN_VARIABLES]
    const env = buildEnvironment('build', inputs(), context())
    for (const key of Object.keys(env)) {
      expect(allowed).toContain(key)
    }
  })

  it('passes a dry run as a flag, not as a variable zup does not read', () => {
    // There was a `ZUP_DRY_RUN` here. Nothing in zup read it, so the action believed
    // it was asking for something and zup was not being asked. A dry run's whole
    // value is that it does not write, and a variable nobody reads does not write
    // and does not stop either.
    expect(buildEnvironment('publish', inputs({ dryRun: true }), context())).not.toHaveProperty(
      'ZUP_DRY_RUN',
    )
  })
})

describe('redaction', () => {
  it('replaces a secret wherever it appears', () => {
    expect(redact(`a ${SECRET} b ${SECRET} c`, [SECRET])).toBe('a *** b *** c')
  })

  it('replaces every secret in a list', () => {
    expect(redact('one two', ['one', 'two'])).toBe('*** ***')
  })

  it('leaves ordinary text alone', () => {
    const text = 'zup 1.4.0 built 3 artifacts in 41s'
    expect(redact(text, [SECRET])).toBe(text)
  })

  it('ignores an empty secret, which would otherwise match everywhere', () => {
    // `split('')` joins every character: an empty token would turn a whole message
    // into asterisks.
    expect(redact('hello', [''])).toBe('hello')
  })

  it('handles a secret containing pattern metacharacters', () => {
    const awkward = 'a.b*c+d?e'
    expect(redact(`x ${awkward} y`, [awkward])).toBe('x *** y')
  })
})

/** A diagnostic shaped by hand, because the point is the annotation not the parse. */
function diagnostic(overrides: Record<string, unknown> = {}) {
  return {
    severity: 'error',
    code: 'zup.manifest.unknown_target',
    message: 'resource references unknown target profile `x64`',
    help: 'use an exact profile id declared under [build.targets]',
    source: null,
    ...overrides,
  } as NonNullable<ReturnType<typeof result>['diagnostics']>[number]
}

describe('annotations', () => {
  it('maps a diagnostic with a source location onto the annotation', () => {
    const { log, lines } = recordingLog()
    annotate(
      result({
        status: 'failure',
        diagnostics: [
          diagnostic({
            source: {
              file: 'zup.toml',
              start_line: 12,
              start_column: 3,
              end_line: 12,
              end_column: 9,
            },
          }),
        ],
      }),
      log,
      '/w',
    )
    const line = lines.find((entry) => entry.startsWith('annotate error')) ?? ''
    expect(line).toContain('zup.manifest.unknown_target')
    expect(line).toContain('use an exact profile id')
    const location = JSON.parse(line.slice(line.indexOf('{'))) as SourceLocation
    expect(location).toEqual({
      file: '/w/zup.toml',
      startLine: 12,
      startColumn: 3,
      endLine: 12,
      endColumn: 9,
    })
  })

  it('maps each severity to its own annotation level', () => {
    const { log, lines } = recordingLog()
    annotate(
      result({
        diagnostics: [
          diagnostic({ severity: 'error', code: 'a', message: 'e' }),
          diagnostic({ severity: 'warning', code: 'b', message: 'w' }),
          diagnostic({ severity: 'notice', code: 'c', message: 'n' }),
        ],
      }),
      log,
      '/w',
    )
    expect(lines.some((line) => line.startsWith('annotate error'))).toBe(true)
    expect(lines.some((line) => line.startsWith('annotate warning'))).toBe(true)
    expect(lines.some((line) => line.startsWith('annotate notice'))).toBe(true)
  })

  it('annotates the same diagnostic once, however many times it arrives', () => {
    // A diagnostic is streamed as it is found and repeated in the final result.
    // Telling the reader the same fact twice is worse than not telling them.
    const { log, lines } = recordingLog()
    const repeated = diagnostic()
    annotate(result({ diagnostics: [repeated, repeated, repeated] }), log, '/w')
    expect(lines.filter((line) => line.startsWith('annotate '))).toHaveLength(1)
  })
  it('resolves a project-relative path against the project', () => {
    // An annotation on a path the runner cannot resolve is invisible, and a project
    // in a subdirectory is the common case.
    const { log, lines } = recordingLog()
    annotate(
      result({
        status: 'failure',
        diagnostics: [
          diagnostic({
            source: {
              file: 'zup.toml',
              start_line: 1,
              start_column: null,
              end_line: null,
              end_column: null,
            },
          }),
        ],
      }),
      log,
      '/w/apps/desktop',
    )
    expect(lines[0]).toContain('"file":"/w/apps/desktop/zup.toml"')
  })

  it('leaves an absolute path alone', () => {
    const { log, lines } = recordingLog()
    annotate(
      result({
        status: 'failure',
        diagnostics: [
          diagnostic({
            source: {
              file: '/abs/zup.toml',
              start_line: 1,
              start_column: null,
              end_line: null,
              end_column: null,
            },
          }),
        ],
      }),
      log,
      '/w',
    )
    expect(lines[0]).toContain('"file":"/abs/zup.toml"')
  })

  it('leaves a windows absolute path alone', () => {
    const { log, lines } = recordingLog()
    annotate(
      result({
        status: 'failure',
        diagnostics: [
          diagnostic({
            source: {
              file: 'C:\\w\\zup.toml',
              start_line: 1,
              start_column: null,
              end_line: null,
              end_column: null,
            },
          }),
        ],
      }),
      log,
      'C:\\w',
    )
    expect(lines[0]).toContain('"file":"C:\\\\w\\\\zup.toml"')
  })

  it('annotates a diagnostic with no location at all', () => {
    const { log, lines } = recordingLog()
    annotate(
      result({
        status: 'failure',
        operation: 'publish.github',
        diagnostics: [
          diagnostic({
            code: 'zup.publish.asset_conflict',
            message: 'v1.4.0 is already published with a different set of assets',
            help: null,
          }),
        ],
      }),
      log,
      '/w',
    )
    expect(lines[0]).toContain('zup.publish.asset_conflict')
    expect(lines[0]).toContain('{}')
  })
})
