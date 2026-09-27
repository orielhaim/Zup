import { describe, expect, it } from 'bun:test'
import type { SourceLocation } from '../src/ports.js'
import { redact } from '../src/runtime.js'
import { annotate, buildEnvironment, INHERITED, TOKEN_VARIABLES } from '../src/workflow.js'
import { context, inputs, recordingLog, SECRET } from './fixtures.js'

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
    // zup reads `GH_TOKEN` then `GITHUB_TOKEN`. Both are set so a developer's
    // local `gh auth` cannot shadow a job-scoped token with a stale one.
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
    expect(
      buildEnvironment('build', inputs(), context({ PATH: '/poisoned' }))['PATH'],
    ).toBeUndefined()
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
    const env = buildEnvironment('build', inputs(), context())
    for (const key of Object.keys(env)) {
      expect([...INHERITED, 'ZUP_DRY_RUN', ...TOKEN_VARIABLES]).toContain(key)
    }
  })

  it('marks a dry run in the environment rather than in a flag', () => {
    // A flag the action chose is a flag zup might not have; an environment variable
    // named `ZUP_DRY_RUN` is the CLI's own contract, so the two cannot disagree.
    expect(buildEnvironment('publish', inputs({ dryRun: true }), context())['ZUP_DRY_RUN']).toBe(
      '1',
    )
  })

  it('does not mark a normal run as a dry run', () => {
    expect(buildEnvironment('publish', inputs(), context())['ZUP_DRY_RUN']).toBeUndefined()
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
    // `split('')` joins every character: an empty token would turn a whole
    // message into asterisks.
    expect(redact('hello', [''])).toBe('hello')
  })

  it('handles a secret containing pattern metacharacters', () => {
    const awkward = 'a.b*c+d?e'
    expect(redact(`x ${awkward} y`, [awkward])).toBe('x *** y')
  })
})

describe('annotations', () => {
  it('maps a diagnostic with a source location onto the annotation', () => {
    const { log, lines } = recordingLog()
    annotate(
      {
        schema: 1,
        operation: 'build',
        success: false,
        targets: [],
        artifacts: [],
        diagnostics: [
          {
            severity: 'error',
            code: 'zup_manifest::unknown_target_profile_reference',
            message: 'resource references unknown target profile `x64`',
            help: 'use an exact profile id declared under [build.targets]',
            source: {
              file: 'zup.toml',
              startLine: 12,
              startColumn: 3,
              endLine: 12,
              endColumn: 9,
            },
          },
        ],
      },
      log,
      '/w',
    )
    const line = lines.find((entry) => entry.startsWith('annotate error')) ?? ''
    expect(line).toContain('zup_manifest::unknown_target_profile_reference')
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
      {
        schema: 1,
        operation: 'build',
        success: true,
        targets: [],
        artifacts: [],
        diagnostics: [
          { severity: 'error', code: 'a', message: 'e' },
          { severity: 'warning', code: 'b', message: 'w' },
          { severity: 'notice', code: 'c', message: 'n' },
        ],
      },
      log,
      '/w',
    )
    expect(lines.some((line) => line.startsWith('annotate error'))).toBe(true)
    expect(lines.some((line) => line.startsWith('annotate warning'))).toBe(true)
    expect(lines.some((line) => line.startsWith('annotate notice'))).toBe(true)
  })

  it('resolves a project-relative path against the project', () => {
    // An annotation on a path the runner cannot resolve is invisible, and a
    // project in a subdirectory is the common case.
    const { log, lines } = recordingLog()
    annotate(
      {
        schema: 1,
        operation: 'build',
        success: false,
        targets: [],
        artifacts: [],
        diagnostics: [
          {
            severity: 'error',
            code: 'a',
            message: 'e',
            source: { file: 'zup.toml', startLine: 1 },
          },
        ],
      },
      log,
      '/w/apps/desktop',
    )
    expect(lines[0]).toContain('"file":"/w/apps/desktop/zup.toml"')
  })

  it('leaves an absolute path alone', () => {
    const { log, lines } = recordingLog()
    annotate(
      {
        schema: 1,
        operation: 'build',
        success: false,
        targets: [],
        artifacts: [],
        diagnostics: [
          {
            severity: 'error',
            code: 'a',
            message: 'e',
            source: { file: '/abs/zup.toml', startLine: 1 },
          },
        ],
      },
      log,
      '/w',
    )
    expect(lines[0]).toContain('"file":"/abs/zup.toml"')
  })

  it('leaves a windows absolute path alone', () => {
    const { log, lines } = recordingLog()
    annotate(
      {
        schema: 1,
        operation: 'build',
        success: false,
        targets: [],
        artifacts: [],
        diagnostics: [
          {
            severity: 'error',
            code: 'a',
            message: 'e',
            source: { file: 'C:\\w\\zup.toml', startLine: 1 },
          },
        ],
      },
      log,
      'C:\\w',
    )
    expect(lines[0]).toContain('"file":"C:\\\\w\\\\zup.toml"')
  })

  it('annotates a diagnostic with no location at all', () => {
    const { log, lines } = recordingLog()
    annotate(
      {
        schema: 1,
        operation: 'publish',
        success: false,
        targets: [],
        artifacts: [],
        diagnostics: [
          {
            severity: 'error',
            code: 'github::PublishedConflict',
            message: 'v1.4.0 is already published with a different set of assets',
          },
        ],
      },
      log,
      '/w',
    )
    expect(lines[0]).toContain('github::PublishedConflict')
    expect(lines[0]).toContain('{}')
  })
})
