import { describe, expect, it } from 'bun:test'
import {
  argumentsFor,
  mergeResults,
  needsToken,
  type Phase,
  phasesFor,
  producesArtifacts,
  releaseManifestPath,
} from '../src/phases.js'
import type { OperationResult } from '../src/result.js'
import { inputs, result } from './fixtures.js'

describe('phasesFor', () => {
  it('maps each operation to the phases it runs', () => {
    expect(phasesFor('setup')).toEqual([])
    expect(phasesFor('build')).toEqual(['build'])
    expect(phasesFor('compose')).toEqual(['compose'])
    expect(phasesFor('attest')).toEqual(['attest'])
    expect(phasesFor('publish')).toEqual(['publish'])
  })

  it('orders release as build, compose, finalize, attest, publish', () => {
    // The order is the contract: finalizing before signing would record
    // pre-signature digests, attesting before finalizing would attest bytes that
    // no longer exist, and publishing before attesting would release something
    // with no provenance.
    expect(phasesFor('release')).toEqual(['build', 'compose', 'finalize', 'attest', 'publish'])
  })

  it('runs no phases for an operation it does not know', () => {
    expect(phasesFor('deploy')).toEqual([])
  })
})

describe('argumentsFor', () => {
  it('builds the zup build command', () => {
    expect(argumentsFor('build', inputs())).toEqual([
      'build',
      '--format',
      'json',
      '--output',
      'dist',
      '--release-manifest',
      'dist/zup-release.json',
    ])
  })

  it('passes each target as its own argument', () => {
    const args = argumentsFor('build', inputs({ targets: ['x64', 'arm64'] }))
    expect(args).toContain('--target')
    expect(args.filter((entry) => entry === '--target')).toHaveLength(2)
    expect(args).toEqual(expect.arrayContaining(['x64', 'arm64']))
  })

  it('passes each artifact as its own argument', () => {
    const args = argumentsFor('build', inputs({ artifacts: ['installer', 'universal'] }))
    expect(args.filter((entry) => entry === '--artifact')).toHaveLength(2)
  })

  it('composes with the staged tree and the release directory', () => {
    expect(argumentsFor('compose', inputs({ releaseDir: 'out' }))).toEqual([
      'publish',
      'stage',
      '--format',
      'json',
      '--output',
      'out/web',
      '--packages',
      'out/packages',
      '--release-dir',
      'out',
    ])
  })

  it('publishes with the release directory, web tree and packages', () => {
    const args = argumentsFor('publish', inputs())
    expect(args.slice(0, 3)).toEqual(['publish', 'github', '--format'])
    expect(args).toEqual(expect.arrayContaining(['--release-dir', 'dist', '--web', 'dist/web']))
    expect(args).toEqual(expect.arrayContaining(['--packages', 'dist/packages']))
  })

  it('passes the publication flags only when they were asked for', () => {
    const plain = argumentsFor('publish', inputs())
    expect(plain).not.toContain('--draft')
    expect(plain).not.toContain('--prerelease')
    expect(plain).not.toContain('--dry-run')
    expect(plain).not.toContain('--tag')
    expect(plain).not.toContain('--repo')

    const full = argumentsFor(
      'publish',
      inputs({ draft: true, prerelease: true, dryRun: true, tag: 'v9.9.9', repo: 'acme/acme' }),
    )
    expect(full).toContain('--draft')
    expect(full).toContain('--prerelease')
    expect(full).toContain('--dry-run')
    expect(full).toEqual(expect.arrayContaining(['--tag', 'v9.9.9']))
    expect(full).toEqual(expect.arrayContaining(['--repo', 'acme/acme']))
  })

  it('passes a custom receipt through', () => {
    const args = argumentsFor('publish', inputs({ receipt: 'out/receipt.json' }))
    expect(args).toEqual(expect.arrayContaining(['--receipt', 'out/receipt.json']))
  })

  it('runs no zup command for attest, which reads a document', () => {
    // zup does not talk to Sigstore. It says which bytes are worth attesting, and
    // `@actions/attest` does the rest.
    expect(argumentsFor('attest', inputs())).toEqual([])
  })

  it('never produces a command string', () => {
    // Every phase's arguments are a vector. A value with a space, a semicolon or a
    // backtick stays exactly one argument, so there is no string for a shell to
    // reinterpret.
    const hostile = inputs({
      targets: ['x64; rm -rf /'],
      releaseDir: 'dist/with space',
      projectPath: '/w/a`b`c',
    })
    const build = argumentsFor('build', hostile)
    expect(build).toContain('x64; rm -rf /')
    expect(build).toContain('dist/with space')
    expect(build).toContain('dist/with space/zup-release.json')

    for (const phase of ['build', 'compose', 'publish'] as Phase[]) {
      const args = argumentsFor(phase, hostile)
      // Nothing was split on whitespace, so a semicolon can only ever be a
      // character inside one argument.
      expect(args.some((entry) => entry === ';')).toBe(false)
      expect(args.some((entry) => entry === 'rm')).toBe(false)
      expect(args.some((entry) => entry === '&&')).toBe(false)
    }
  })
})

describe('advanced arguments', () => {
  it('appends advanced arguments last, so a typed input can be overridden', () => {
    const args = argumentsFor('build', inputs({ args: ['--force'] }))
    expect(args[args.length - 1]).toBe('--force')
    expect(args).toContain('build')
  })

  it('leaves the arguments unchanged when there are none', () => {
    expect(argumentsFor('build', inputs())).toEqual([
      'build',
      '--format',
      'json',
      '--output',
      'dist',
      '--release-manifest',
      'dist/zup-release.json',
    ])
  })
})

describe('phase properties', () => {
  it('gives the token to exactly one phase', () => {
    const withToken = (['setup', 'build', 'compose', 'attest', 'publish', 'release'] as const)
      .flatMap(phasesFor)
      .filter(needsToken)
    expect(new Set(withToken)).toEqual(new Set(['publish']))
  })

  it('marks the phases that produce an uploadable release directory', () => {
    expect(phasesFor('release').filter(producesArtifacts)).toEqual(['build', 'compose', 'finalize'])
  })

  it('names the release manifest under the release directory', () => {
    expect(releaseManifestPath('dist')).toBe('dist/zup-release.json')
    expect(releaseManifestPath('out/nested')).toBe('out/nested/zup-release.json')
  })
})

describe('mergeResults', () => {
  it('returns nothing when no phase produced a result', () => {
    expect(mergeResults(['build'], new Map())).toBeUndefined()
  })

  it('combines targets and artifacts across a release', () => {
    const merged = mergeResults(
      ['build', 'compose', 'publish'],
      new Map<Phase, OperationResult>([
        [
          'build',
          result({
            targets: ['x64'],
            artifacts: [{ path: 'a', digest: 'a', size: 1, kind: 'k', mode: 'm' }],
          }),
        ],
        ['compose', result({ operation: 'compose', targets: ['arm64'] })],
        [
          'publish',
          result({
            operation: 'publish',
            release: {
              repository: 'acme/acme',
              host: 'github.com',
              tag: 'v1.4.0',
              releaseId: 1,
              state: 'published',
              assets: [],
            },
          }),
        ],
      ]),
    )
    expect(merged?.targets).toEqual(['x64', 'arm64'])
    expect(merged?.artifacts).toHaveLength(1)
    expect(merged?.release?.tag).toBe('v1.4.0')
    expect(merged?.operation).toBe('release')
  })

  it('fails the whole run when any phase failed', () => {
    const merged = mergeResults(
      ['build', 'compose'],
      new Map<Phase, OperationResult>([
        ['build', result({ success: true })],
        ['compose', result({ operation: 'compose', success: false })],
      ]),
    )
    expect(merged?.success).toBe(false)
  })

  it('de-duplicates a target two phases both reported', () => {
    const merged = mergeResults(
      ['build', 'compose'],
      new Map<Phase, OperationResult>([
        ['build', result({ targets: ['x64'] })],
        ['compose', result({ operation: 'compose', targets: ['x64', 'arm64'] })],
      ]),
    )
    expect(merged?.targets).toEqual(['x64', 'arm64'])
  })
})
