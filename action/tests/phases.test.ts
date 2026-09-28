import { describe, expect, it } from 'bun:test'
import {
  argumentsFor,
  needsToken,
  operationFor,
  type Phase,
  phasesFor,
  producesArtifacts,
  releaseManifestPath,
  signingPlanPath,
} from '../src/phases.js'
import { parseResult } from '../src/protocol.js'
import { mergeResults } from '../src/summary.js'
import { inputs } from './fixtures.js'
import { fixture } from './protocol-fixtures.js'

describe('phasesFor', () => {
  it('maps each operation to the phases it runs', () => {
    expect(phasesFor('setup')).toEqual([])
    expect(phasesFor('build')).toEqual(['build'])
    expect(phasesFor('compose')).toEqual(['compose'])
    expect(phasesFor('finalize')).toEqual(['finalize'])
    expect(phasesFor('attest')).toEqual(['attest'])
    expect(phasesFor('publish')).toEqual(['publish'])
  })

  it('orders release as build, compose, finalize, attest, publish', () => {
    // The order is the contract: finalizing before signing would record
    // pre-signature digests, attesting before finalizing would attest bytes that no
    // longer exist, and publishing before attesting would release something with no
    // provenance.
    expect(phasesFor('release')).toEqual(['build', 'compose', 'finalize', 'attest', 'publish'])
  })

  it('runs no phases for an operation it does not know', () => {
    expect(phasesFor('deploy')).toEqual([])
  })
})

describe('operationFor', () => {
  it("names the zup operation each phase runs, in zup's vocabulary", () => {
    // The two vocabularies are not the same, and conflating them is how the action
    // and the CLI came to disagree about what a step did.
    expect(operationFor('build')).toBe('build')
    expect(operationFor('compose')).toBe('publish.stage')
    expect(operationFor('finalize')).toBe('sign.verify')
    expect(operationFor('publish')).toBe('publish.github')
  })

  it('has no operation for attest, because zup does not talk to Sigstore', () => {
    // zup owns which bytes are worth attesting and says so in the release
    // description. The token exchange and the signature format are GitHub's and
    // Sigstore's, so a `zup attest` verb would be a second signer with a second set
    // of bugs.
    expect(operationFor('attest')).toBeUndefined()
  })
})

describe('argumentsFor', () => {
  it('builds the zup build command in the streaming format', () => {
    // `jsonl` rather than `json`, because a build that reports a failing check in
    // the first second is a build somebody can stop.
    expect(argumentsFor('build', inputs())).toEqual([
      'build',
      '--output',
      'dist',
      '--release-manifest',
      'dist/zup-release.json',
      '--format',
      'jsonl',
    ])
  })

  it('passes each target as its own argument', () => {
    const args = argumentsFor('build', inputs({ targets: ['x64', 'arm64'] }))
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
      '--output',
      'out/web',
      '--packages',
      'out/packages',
      '--release-dir',
      'out',
      '--format',
      'jsonl',
    ])
  })

  it('does not pass --dry-run to `publish stage`, which has no such flag', () => {
    // It used to. A dry-run `release` therefore failed in its second phase with a
    // usage error, and the error named a flag the developer had never typed.
    const args = argumentsFor('compose', inputs({ dryRun: true }))
    expect(args).not.toContain('--dry-run')
  })

  it('passes --dry-run to `publish github`, which does', () => {
    // The publication is the only step that writes somewhere else, so it is the
    // only step a dry run has to hold back.
    expect(argumentsFor('publish', inputs({ dryRun: true }))).toContain('--dry-run')
  })

  it('publishes with the release directory, web tree and packages', () => {
    const args = argumentsFor('publish', inputs())
    expect(args.slice(0, 2)).toEqual(['publish', 'github'])
    expect(args).toEqual(expect.arrayContaining(['--release-dir', 'dist', '--web', 'dist/web']))
    expect(args).toEqual(expect.arrayContaining(['--packages', 'dist/packages']))
  })

  it('passes the publication flags only when they were asked for', () => {
    const plain = argumentsFor('publish', inputs())
    for (const flag of ['--draft', '--prerelease', '--dry-run', '--tag', '--repo', '--receipt']) {
      expect(plain).not.toContain(flag)
    }

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

  it('passes the signing flags to `sign verify`', () => {
    const args = argumentsFor('finalize', inputs({ allowUnsigned: true, onlineRevocation: true }))
    expect(args).toEqual(
      expect.arrayContaining(['sign', 'verify', '--allow-unsigned', '--online-revocation']),
    )
    expect(argumentsFor('finalize', inputs())).not.toContain('--allow-unsigned')
  })

  it('passes a custom receipt through', () => {
    const args = argumentsFor('publish', inputs({ receipt: 'out/receipt.json' }))
    expect(args).toEqual(expect.arrayContaining(['--receipt', 'out/receipt.json']))
  })

  it('runs no zup command for attest, which reads a document', () => {
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
      for (const fragment of [';', 'rm', '&&', '`']) {
        expect(args).not.toContain(fragment)
      }
    }
  })
})

describe('advanced arguments', () => {
  it('appends advanced arguments last, so a typed input can be overridden', () => {
    const args = argumentsFor('build', inputs({ args: ['--force'] }))
    expect(args.at(-1)).toBe('--force')
    expect(args).toContain('build')
  })

  it('leaves the arguments unchanged when there are none', () => {
    expect(argumentsFor('build', inputs()).at(-1)).toBe('jsonl')
  })
})

describe('phase properties', () => {
  it('gives the token to exactly one phase', () => {
    const withToken = (['build', 'compose', 'finalize', 'attest', 'publish', 'release'] as const)
      .flatMap(phasesFor)
      .filter(needsToken)
    expect(new Set(withToken)).toEqual(new Set(['publish']))
  })

  it('marks the phases that produce an uploadable release directory', () => {
    expect(phasesFor('release').filter(producesArtifacts)).toEqual(['build', 'compose', 'finalize'])
  })

  it('names the release description and the signing plan under the release directory', () => {
    expect(releaseManifestPath('dist')).toBe('dist/zup-release.json')
    expect(releaseManifestPath('out/nested')).toBe('out/nested/zup-release.json')
    expect(signingPlanPath('dist')).toBe('dist/zup-signing.json')
  })
})

describe('mergeResults', () => {
  it('returns nothing when no phase produced a result', () => {
    expect(mergeResults(['build'], new Map())).toBeUndefined()
  })

  it('unions the fields the phases reported, keeping the later artifact', () => {
    // The later value has to win: `sign verify` rewrites the release description
    // with the *published* digests, and a summary showing the pre-signature ones
    // would show bytes no downloader receives.
    const merged = mergeResults(
      ['build', 'compose', 'publish'],
      new Map<Phase, ReturnType<typeof parseResult>>([
        ['build', parseResult(fixture('build-success'))],
        ['compose', parseResult(fixture('publish-stage'))],
        ['publish', parseResult(fixture('publish-success'))],
      ]),
    )
    expect(merged?.application?.version).toBe('1.4.0')
    expect(merged?.targets.map((target) => target.profile)).toEqual(['windows-x64'])
    expect(merged?.artifacts.map((artifact) => artifact.path)).toEqual([
      'Acme-Windows-Setup.exe',
      'Acme-Windows-x64.zup',
    ])
    expect(merged?.publication?.tag).toBe('v1.4.0')
    expect(merged?.release_manifest).toBe('dist/zup-release.json')
  })

  it('claims no operation of its own, because zup has no word for a workflow', () => {
    const merged = mergeResults(
      ['build'],
      new Map<Phase, ReturnType<typeof parseResult>>([
        ['build', parseResult(fixture('build-success'))],
      ]),
    )
    // An empty string rather than `release`: the field says what zup did, and no zup
    // operation is called `release`.
    expect(merged?.operation).toBe('')
  })

  it('fails the whole run when any phase failed', () => {
    const merged = mergeResults(
      ['build', 'publish'],
      new Map<Phase, ReturnType<typeof parseResult>>([
        ['build', parseResult(fixture('build-success'))],
        ['publish', parseResult(fixture('publish-conflict'))],
      ]),
    )
    expect(merged?.status).toBe('failure')
  })

  it('de-duplicates a target two phases both reported', () => {
    const merged = mergeResults(
      ['build', 'publish'],
      new Map<Phase, ReturnType<typeof parseResult>>([
        ['build', parseResult(fixture('build-success'))],
        ['publish', parseResult(fixture('publish-success'))],
      ]),
    )
    expect(merged?.targets).toHaveLength(1)
  })
})
