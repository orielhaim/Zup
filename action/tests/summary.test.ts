import { describe, expect, it } from 'bun:test'

import type { OperationResult } from '../src/result.js'
import { formatBytes, renderSummary, type Summary } from '../src/summary.js'
import type { ResolvedTool } from '../src/tool.js'

const TOOL: ResolvedTool = {
  path: '/opt/hostedtoolcache/zup/1.4.0/x64/zup-linux-x64',
  version: '1.4.0',
  source: 'cache',
  identity: { platform: 'linux', arch: 'x64' },
}

function result(overrides: Partial<OperationResult> = {}): OperationResult {
  return {
    schema: 1,
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
      },
      {
        path: 'Acme-Web-Setup.exe',
        digest: 'b'.repeat(64),
        size: 2_097_152,
        kind: 'installer',
        mode: 'standalone',
      },
    ],
    releaseManifest: 'dist/zup-release.json',
    diagnostics: [],
    ...overrides,
  }
}

function summary(overrides: Partial<Summary> = {}): Summary {
  return {
    operation: 'release',
    tool: TOOL,
    performed: ['build', 'compose', 'attest', 'publish'],
    result: result(),
    failure: undefined,
    dryRun: false,
    ...overrides,
  }
}

describe('formatBytes', () => {
  it('uses binary units, matching what a file manager shows', () => {
    expect(formatBytes(0)).toBe('0 B')
    expect(formatBytes(1023)).toBe('1023 B')
    expect(formatBytes(1024)).toBe('1.0 KiB')
    expect(formatBytes(248_512_896)).toBe('237 MiB')
    expect(formatBytes(2_097_152)).toBe('2.0 MiB')
  })

  it('drops the decimal for a large value, which is noise', () => {
    expect(formatBytes(5 * 1024 ** 3)).toBe('5.0 GiB')
    expect(formatBytes(900 * 1024 ** 3)).toBe('900 GiB')
  })

  it('refuses to render a number it cannot trust', () => {
    expect(formatBytes(-1)).toBe('—')
    expect(formatBytes(Number.NaN)).toBe('—')
  })
})

describe('build summary', () => {
  it('leads with the version, the targets and the artifact count', () => {
    const text = renderSummary(summary())
    expect(text).toContain('## zup')
    expect(text).toContain('| Version | 1.4.0 |')
    expect(text).toContain('| Targets | windows-x64, windows-arm64 |')
    expect(text).toContain('| Artifacts | 2 |')
  })

  it('lists each artifact with its size and a truncated digest', () => {
    const text = renderSummary(summary())
    expect(text).toContain('Acme-Windows-Setup.exe')
    expect(text).toContain('237 MiB')
    expect(text).toContain('Acme-Web-Setup.exe')
    expect(text).toContain('2.0 MiB')
    // A 64-character hex string defeats a table a human reads.
    expect(text).toContain(`${'a'.repeat(16)}…`)
    expect(text).not.toContain('a'.repeat(64))
  })

  it('reports where the zup CLI came from', () => {
    // A developer wondering why a build was slow needs this answer.
    const text = renderSummary(summary())
    expect(text).toContain('zup CLI')
    expect(text).toContain('1.4.0 (linux-x64, cache)')
  })

  it('shows a download when the cache missed', () => {
    const text = renderSummary(summary({ tool: { ...TOOL, source: 'download' } }))
    expect(text).toContain('linux-x64, download')
  })

  it('links the release manifest so the digests are reachable', () => {
    expect(renderSummary(summary())).toContain('dist/zup-release.json')
  })

  it('does not dump internal JSON', () => {
    const text = renderSummary(summary())
    expect(text).not.toContain('"schema"')
    expect(text).not.toContain('"artifacts":')
  })

  it('carries no timestamp, so two runs of the same build look the same', () => {
    // Volatile output makes a summary impossible to diff and impossible to
    // snapshot, for no benefit.
    const text = renderSummary(summary())
    expect(text).not.toMatch(/\d{4}-\d{2}-\d{2}T/u)
  })
})

describe('release summary', () => {
  it('reports the release, its repository, its state and its URL', () => {
    const text = renderSummary(
      summary({
        operation: 'publish',
        result: result({
          operation: 'publish',
          release: {
            repository: 'acme/acme',
            host: 'github.com',
            tag: 'v1.4.0',
            releaseId: 1234,
            state: 'published',
            url: 'https://github.com/acme/acme/releases/tag/v1.4.0',
            immutable: true,
            assets: [{ name: 'Acme.exe', size: 10, digest: 'c'.repeat(64), state: 'uploaded' }],
          },
        }),
      }),
    )
    expect(text).toContain('| Release | `v1.4.0` |')
    expect(text).toContain('| Repository | acme/acme |')
    expect(text).toContain('| Status | published |')
    expect(text).toContain('| Immutable | yes |')
    expect(text).toContain('https://github.com/acme/acme/releases/tag/v1.4.0')
    expect(text).toContain('| Assets | 1 |')
  })

  it('says "no" for immutability rather than omitting the row', () => {
    // An absent row reads as "not reported"; a false one is a fact.
    const text = renderSummary(
      summary({
        result: result({
          release: {
            repository: 'acme/acme',
            host: 'github.com',
            tag: 'v1.4.0',
            releaseId: 1,
            state: 'draft',
            immutable: false,
            assets: [],
          },
        }),
      }),
    )
    expect(text).toContain('| Immutable | no |')
  })

  it('omits immutability when the host did not report it', () => {
    const text = renderSummary(
      summary({
        result: result({
          release: {
            repository: 'acme/acme',
            host: 'ghe.acme.internal',
            tag: 'v1.4.0',
            releaseId: 1,
            state: 'published',
            assets: [],
          },
        }),
      }),
    )
    expect(text).not.toContain('Immutable')
  })

  it('shows a dash rather than an empty cell for an unknown version', () => {
    const text = renderSummary(summary({ result: result({ appVersion: undefined }) }))
    expect(text).toContain('| Version | — |')
  })
})

describe('failure summary', () => {
  it('says what failed and what to do', () => {
    const text = renderSummary(
      summary({
        operation: 'build',
        failure: {
          message: 'zup build exited with code 101: no such file or directory',
          remedy: 'Read the output above.',
        },
      }),
    )
    expect(text).toContain('**build failed.**')
    expect(text).toContain('exited with code 101')
    expect(text).toContain('Read the output above.')
  })

  it('omits the artifact table, which would be noise on a failure', () => {
    const text = renderSummary(summary({ failure: { message: 'm', remedy: 'r' } }))
    expect(text).not.toContain('Acme-Windows-Setup.exe')
  })

  it('still reports which zup was in use', () => {
    // The first question about a failed release is usually "which version".
    const text = renderSummary(summary({ failure: { message: 'm', remedy: 'r' } }))
    expect(text).toContain('zup CLI')
    expect(text).toContain('1.4.0 (cache)')
  })
})
