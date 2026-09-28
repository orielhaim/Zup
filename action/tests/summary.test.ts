import { describe, expect, it } from 'bun:test'
import type { Phase } from '../src/phases.js'
import { parseResult } from '../src/protocol.js'
import { formatBytes, mergeResults, renderSummary, type Summary } from '../src/summary.js'
import type { ResolvedTool } from '../src/tool.js'
import { fixture } from './protocol-fixtures.js'

const TOOL: ResolvedTool = {
  path: '/opt/hostedtoolcache/zup/1.4.0/x64/zup-linux-x64',
  version: '1.4.0',
  source: 'cache',
  identity: { platform: 'linux', arch: 'x64' },
}

/** The results a `release` run produces, straight from the fixtures zup generated. */
function releaseResults(): Map<Phase, ReturnType<typeof parseResult>> {
  return new Map<Phase, ReturnType<typeof parseResult>>([
    ['build', parseResult(fixture('build-success'))],
    ['compose', parseResult(fixture('publish-stage'))],
    ['publish', parseResult(fixture('publish-success'))],
  ])
}

function summary(overrides: Partial<Summary> = {}): Summary {
  const results = releaseResults()
  return {
    operation: 'release',
    tool: TOOL,
    performed: ['build', 'compose', 'publish'],
    results,
    result: mergeResults(['build', 'compose', 'publish'], results),
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
    expect(text).toContain('| Targets | windows-x64 |')
    expect(text).toContain('| Artifacts | 2 |')
  })

  it('lists each artifact with its size, its signature and a truncated digest', () => {
    const text = renderSummary(summary())
    expect(text).toContain('Acme-Windows-Setup.exe')
    expect(text).toContain('237 MiB')
    expect(text).toContain('Acme-Windows-x64.zup')
    expect(text).toContain('18.0 MiB')
    // A 64-character hex string defeats a table a human reads.
    expect(text).toContain(`${'a'.repeat(16)}…`)
    expect(text).not.toContain('a'.repeat(64))
  })

  it('marks an artifact nobody has looked at, rather than calling it unsigned', () => {
    // `—` rather than a tick: the release has not been signed *by this action's
    // account of it*, and claiming either answer would be a claim.
    expect(renderSummary(summary())).toContain('| — |')
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

  it('names the release description so the digests are reachable', () => {
    expect(renderSummary(summary())).toContain('dist/zup-release.json')
  })

  it('does not dump the machine document', () => {
    // A summary that dumps the protocol is why people stop opening summaries.
    const text = renderSummary(summary())
    expect(text).not.toContain('"protocol"')
    expect(text).not.toContain('"artifacts":')
  })

  it('carries no timestamp, so two runs of the same build look the same', () => {
    // Volatile output makes a summary impossible to diff and impossible to snapshot,
    // for no benefit.
    expect(renderSummary(summary())).not.toMatch(/\d{4}-\d{2}-\d{2}T/u)
  })
})

describe('release summary', () => {
  it('reports the release, its repository, its state and its URL', () => {
    const text = renderSummary(summary({ operation: 'publish' }))
    expect(text).toContain('| Release | `v1.4.0` |')
    expect(text).toContain('| Repository | acme/acme |')
    expect(text).toContain('| Status | published |')
    expect(text).toContain('| Immutable | yes |')
    expect(text).toContain('https://github.com/acme/acme/releases/tag/v1.4.0')
    expect(text).toContain('| Assets | 1 |')
  })

  it('says "no" for immutability rather than omitting the row', () => {
    // An absent row reads as "not reported"; a false one is a fact.
    const published = parseResult(fixture('publish-success'), 'publish.github')
    const text = renderSummary(
      summary({
        result: {
          ...published,
          publication: { ...published.publication!, immutable: false },
        },
      }),
    )
    expect(text).toContain('| Immutable | no |')
  })

  it('omits immutability when the host did not report it', () => {
    const published = parseResult(fixture('publish-success'), 'publish.github')
    const text = renderSummary(
      summary({
        result: { ...published, publication: { ...published.publication!, immutable: null } },
      }),
    )
    expect(text).not.toContain('Immutable')
  })

  it('shows a dash rather than an empty cell for an unknown version', () => {
    // A toolchain report has no application: it is about this machine, not a
    // project. `—` says "there is none" rather than inventing a version.
    const status = parseResult(fixture('toolchain-status'), 'toolchain.status')
    expect(renderSummary(summary({ result: status, results: new Map() }))).toContain(
      '| Version | — |',
    )
  })
})

describe('failure summary', () => {
  it('says what failed and what to do', () => {
    const text = renderSummary(
      summary({
        operation: 'build',
        failure: {
          message: 'zup build failed (exit code 1): zup.manifest.unknown_target: no profile',
          remedy: 'Every diagnostic above is annotated with its code and location.',
        },
      }),
    )
    expect(text).toContain('**build failed.**')
    expect(text).toContain('zup.manifest.unknown_target')
    expect(text).toContain('Every diagnostic above is annotated')
  })

  it('keeps the artifact table, because a partial release is the case worth a record', () => {
    // Four installers and then a signature failure is exactly the release somebody
    // needs to look at tomorrow. Dropping the table is how that becomes a
    // one-line "failed".
    const build = parseResult(fixture('build-success'), 'build')
    const text = renderSummary(
      summary({ failure: { message: 'm', remedy: 'r' }, result: build, results: new Map() }),
    )
    expect(text).toContain('Acme-Windows-Setup.exe')
    expect(text).toContain('dist/zup-release.json')
  })

  it('still reports which zup was in use', () => {
    // The first question about a failed release is usually "which version".
    const text = renderSummary(summary({ failure: { message: 'm', remedy: 'r' } }))
    expect(text).toContain('zup CLI')
    expect(text).toContain('1.4.0 (cache)')
  })

  it('says the step failed when it failed before any phase ran', () => {
    const text = renderSummary(
      summary({ result: undefined, results: new Map(), failure: { message: 'm', remedy: 'r' } }),
    )
    expect(text).not.toContain('| Version |')
    expect(text).toContain('**release failed.**')
  })
})
