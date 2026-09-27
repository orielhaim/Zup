import { describe, expect, it } from 'bun:test'

import { parseResult, RESULT_SCHEMA, ResultFormatError } from '../src/result.js'

/** A well-formed build result, as zup emits it. */
function buildResult(overrides: Record<string, unknown> = {}): string {
  return JSON.stringify({
    schema: RESULT_SCHEMA,
    operation: 'build',
    success: true,
    appVersion: '1.4.0',
    targets: ['windows-x64', 'windows-arm64'],
    artifacts: [
      {
        path: 'Acme-Windows-Setup.exe',
        digest: 'a'.repeat(64),
        size: 248_512_896,
        kind: 'installer',
        mode: 'standalone',
        signature: 'signed',
      },
    ],
    releaseManifest: 'dist/zup-release.json',
    release: null,
    diagnostics: [],
    summary: '2 targets, 2 artifacts',
    ...overrides,
  })
}

describe('parseResult', () => {
  it('reads a build result', () => {
    const result = parseResult(buildResult(), 'build')
    expect(result.success).toBe(true)
    expect(result.appVersion).toBe('1.4.0')
    expect(result.targets).toEqual(['windows-x64', 'windows-arm64'])
    expect(result.artifacts[0]?.path).toBe('Acme-Windows-Setup.exe')
    expect(result.artifacts[0]?.size).toBe(248_512_896)
    expect(result.releaseManifest).toBe('dist/zup-release.json')
  })

  it('reads a publish result with a release', () => {
    const result = parseResult(
      buildResult({
        operation: 'publish',
        release: {
          repository: 'acme/acme',
          host: 'github.com',
          tag: 'v1.4.0',
          releaseId: 1234,
          state: 'published',
          url: 'https://github.com/acme/acme/releases/tag/v1.4.0',
          immutable: true,
          assets: [{ name: 'Acme.exe', size: 10, digest: 'b'.repeat(64), state: 'uploaded' }],
        },
      }),
      'publish',
    )
    expect(result.release?.releaseId).toBe(1234)
    expect(result.release?.immutable).toBe(true)
    expect(result.release?.assets[0]?.name).toBe('Acme.exe')
  })

  it('reads diagnostics with source locations', () => {
    const result = parseResult(
      buildResult({
        success: false,
        diagnostics: [
          {
            severity: 'error',
            code: 'zup_manifest::invalid',
            message: 'the build target matrix must contain at least one profile',
            help: 'declare a profile under [build.targets.<profile>]',
            source: {
              file: 'zup.toml',
              startLine: 4,
              startColumn: 1,
              endLine: 4,
              endColumn: 30,
            },
          },
        ],
      }),
      'build',
    )
    const diagnostic = result.diagnostics[0]
    expect(diagnostic?.severity).toBe('error')
    expect(diagnostic?.code).toBe('zup_manifest::invalid')
    expect(diagnostic?.help).toContain('[build.targets')
    expect(diagnostic?.source).toEqual({
      file: 'zup.toml',
      startLine: 4,
      startColumn: 1,
      endLine: 4,
      endColumn: 30,
    })
  })

  it('reads a warning and a notice', () => {
    const result = parseResult(
      buildResult({
        diagnostics: [
          { severity: 'warning', code: 'zup_build::deferred', message: 'signing was skipped' },
          { severity: 'notice', code: 'zup_build::cached', message: 'reused a cached target' },
        ],
      }),
      'build',
    )
    expect(result.diagnostics.map((entry) => entry.severity)).toEqual(['warning', 'notice'])
  })

  it('treats an unknown severity as a notice rather than an error', () => {
    // A diagnostic zup invented a level for must not fail a release that
    // otherwise succeeded.
    const result = parseResult(
      buildResult({ diagnostics: [{ severity: 'critical', code: 'x', message: 'm' }] }),
      'build',
    )
    expect(result.diagnostics[0]?.severity).toBe('notice')
  })

  it('survives a log line printed before the document', () => {
    // Streaming a build and capturing its result means other output can
    // interleave. A build that worked must not fail because something else was
    // printed.
    const result = parseResult(`zup: compiling\n${buildResult()}\n`, 'build')
    expect(result.appVersion).toBe('1.4.0')
  })

  it('tolerates missing optional fields', () => {
    const result = parseResult(
      JSON.stringify({ schema: 1, operation: 'compose', success: true }),
      'compose',
    )
    expect(result.appVersion).toBeUndefined()
    expect(result.targets).toEqual([])
    expect(result.artifacts).toEqual([])
    expect(result.diagnostics).toEqual([])
  })

  it('refuses empty stdout', () => {
    // zup printed its error to stderr and exited non-zero; the caller reports the
    // exit code, and this is the message that says why there is nothing to read.
    expect(() => parseResult('   \n', 'build')).toThrow(ResultFormatError)
  })

  it('refuses a schema it does not understand', () => {
    expect(() => parseResult(buildResult({ schema: 2 }), 'build')).toThrow(
      /schema is 2 and this action understands 1/u,
    )
  })

  it('refuses a result for a different operation', () => {
    // Running `zup build` and being handed a publish envelope would mean the
    // binary is not the one the action thinks it is.
    expect(() => parseResult(buildResult({ operation: 'publish' }), 'build')).toThrow(
      /expected a `build` result and got `publish`/u,
    )
  })

  it('refuses a truncated document and shows where it broke', () => {
    const truncated = buildResult().slice(0, 40)
    expect(() => parseResult(truncated, 'build')).toThrow(/did not emit a readable result/u)
  })

  it('refuses a document that is not an object', () => {
    expect(() => parseResult('[1,2,3]', 'build')).toThrow(/no JSON object in stdout/u)
    expect(() => parseResult('42', 'build')).toThrow(/no JSON object in stdout/u)
  })

  it('refuses a document with no schema', () => {
    expect(() => parseResult('{"operation":"build"}', 'build')).toThrow(/no numeric `schema`/u)
  })

  it('names the output it saw, so a bug report is actionable', () => {
    try {
      parseResult('not json at all', 'build')
      expect.unreachable('a non-JSON stdout must be refused')
    } catch (error) {
      expect((error as Error).message).toContain('not json at all')
    }
  })

  it('truncates a long preview rather than pasting a build log into an error', () => {
    const noise = 'x'.repeat(5000)
    try {
      parseResult(noise, 'build')
      expect.unreachable('a non-JSON stdout must be refused')
    } catch (error) {
      const message = (error as Error).message
      expect(message.length).toBeLessThan(400)
      expect(message).not.toContain('x'.repeat(300))
    }
  })

  it('skips an artifact with no path or digest rather than inventing one', () => {
    const result = parseResult(
      buildResult({
        artifacts: [
          { path: 'ok.exe', digest: 'a'.repeat(64), size: 1, kind: 'k', mode: 'm' },
          { digest: 'b'.repeat(64), size: 1 },
          'not an object',
        ],
      }),
      'build',
    )
    expect(result.artifacts).toHaveLength(1)
  })
})
