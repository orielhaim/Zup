/**
 * The phases, in the order they run, and how each one is invoked.
 *
 * Release logic stays in the CLI. This module builds argument vectors and reads
 * envelopes; it does not decide how a target is built, what a release is, or what
 * gets attested. A phase that reimplemented release logic here would be a second
 * publisher with a second set of bugs, against a host that misbehaves.
 *
 * ```text
 * build -> compose -> sign (the project's own step) -> finalize -> attest -> publish
 * ```
 *
 * Each boundary is there because of one specific way the order can be wrong:
 *
 * - **sign after finalize** records pre-signature digests, so the release
 *   description describes bytes that do not exist.
 * - **attest before signing** attests a file that signing is about to change.
 * - **publish before verifying** puts out a release and then discovers nobody
 *   signed it.
 *
 * `finalize` closes the first two. It reads the signing plan, checks every
 * signature, and rewrites the release description with the identity that will
 * actually be published.
 */

import type { Inputs } from './inputs.js'
import type { OperationResult } from './result.js'

/** The zup operations this action can invoke. */
export type Phase = 'build' | 'compose' | 'finalize' | 'attest' | 'publish'

/** The release manifest file name zup writes. */
export const RELEASE_MANIFEST_NAME = 'zup-release.json'

/** The signing plan file name zup writes beside the release manifest. */
export const SIGNING_PLAN_NAME = 'zup-signing.json'

/**
 * Which zup commands a workflow `operation` expands to.
 *
 * `release` is a convenience over the phases a hand-written pipeline would run, in
 * the same order: a loop over this table, not a separate code path.
 */
export function phasesFor(operation: string): Phase[] {
  switch (operation) {
    case 'build':
      return ['build']
    case 'compose':
      return ['compose']
    case 'finalize':
      return ['finalize']
    case 'attest':
      return ['attest']
    case 'publish':
      return ['publish']
    case 'release':
      return ['build', 'compose', 'finalize', 'attest', 'publish']
    default:
      return []
  }
}

/**
 * The argument vector for one phase, advanced arguments last.
 *
 * Every value goes through as a separate argument, so a project directory with a
 * space, an ampersand or a backtick in it is just a directory. `inputs.args` goes
 * last so a typed input can be overridden.
 */
export function argumentsFor(phase: Phase, inputs: Inputs): string[] {
  return [...phaseArguments(phase, inputs), ...inputs.args]
}

function phaseArguments(phase: Phase, inputs: Inputs): string[] {
  switch (phase) {
    case 'build': {
      const args = ['build', '--format', 'json']
      for (const target of inputs.targets) {
        args.push('--target', target)
      }
      for (const artifact of inputs.artifacts) {
        args.push('--artifact', artifact)
      }
      args.push('--output', inputs.releaseDir)
      args.push('--release-manifest', releaseManifestPath(inputs.releaseDir))
      return args
    }
    case 'compose': {
      const args = [
        'publish',
        'stage',
        '--format',
        'json',
        '--output',
        `${inputs.releaseDir}/web`,
        '--packages',
        `${inputs.releaseDir}/packages`,
        '--release-dir',
        inputs.releaseDir,
      ]
      if (inputs.dryRun) {
        args.push('--dry-run')
      }
      return args
    }
    case 'finalize': {
      const args = ['sign', 'verify', '--release-dir', inputs.releaseDir]
      if (inputs.allowUnsigned) {
        args.push('--allow-unsigned')
      }
      if (inputs.onlineRevocation) {
        args.push('--online-revocation')
      }
      return args
    }
    case 'attest':
      // `attest` is not a zup subcommand. zup does not talk to Sigstore and should
      // not: OIDC token exchange is GitHub's and the signature format is
      // Sigstore's. What zup owns is which bytes are worth attesting, and it says
      // so in the release manifest, which the action reads itself.
      return []
    case 'publish': {
      const args = [
        'publish',
        'github',
        '--format',
        'json',
        '--release-dir',
        inputs.releaseDir,
        '--web',
        `${inputs.releaseDir}/web`,
        '--packages',
        `${inputs.releaseDir}/packages`,
      ]
      if (inputs.receipt !== undefined) {
        args.push('--receipt', inputs.receipt)
      }
      if (inputs.repo !== undefined) {
        args.push('--repo', inputs.repo)
      }
      if (inputs.tag !== undefined) {
        args.push('--tag', inputs.tag)
      }
      if (inputs.draft) {
        args.push('--draft')
      }
      if (inputs.prerelease) {
        args.push('--prerelease')
      }
      if (inputs.dryRun) {
        args.push('--dry-run')
      }
      return args
    }
  }
}

/** The release manifest path, relative to the project. */
export function releaseManifestPath(releaseDir: string): string {
  return `${releaseDir}/${RELEASE_MANIFEST_NAME}`
}

/** The signing plan path, relative to the project. */
export function signingPlanPath(releaseDir: string): string {
  return `${releaseDir}/${SIGNING_PLAN_NAME}`
}

/** Whether a phase is the one that needs the publish credential. */
export function needsToken(phase: Phase): boolean {
  return phase === 'publish'
}

/** Whether a phase produces a release directory worth uploading. */
export function producesArtifacts(phase: Phase): boolean {
  return phase === 'build' || phase === 'compose' || phase === 'finalize'
}

/** Merge two results of one `release` run into the one that is reported. */
export function mergeResults(
  phases: Phase[],
  results: Map<Phase, OperationResult>,
): OperationResult | undefined {
  let merged: OperationResult | undefined
  for (const phase of phases) {
    const result = results.get(phase)
    if (result === undefined) {
      continue
    }
    if (merged === undefined) {
      merged = { ...result, operation: 'release' }
      continue
    }
    // Bound to a `const` so the narrowing survives into the callbacks below: a
    // `let` is not narrowed inside a closure, and the artifact merge is two
    // closures.
    const previous = merged
    merged = {
      ...previous,
      operation: 'release',
      success: previous.success && result.success,
      targets: [...new Set([...previous.targets, ...result.targets])],
      artifacts: [
        ...new Map(previous.artifacts.map((a) => [a.path, a])).values(),
        ...result.artifacts.filter((a) => !previous.artifacts.some((b) => b.path === a.path)),
      ],
      diagnostics: [...previous.diagnostics, ...result.diagnostics],
      releaseManifest: result.releaseManifest ?? previous.releaseManifest,
      appVersion: result.appVersion ?? previous.appVersion,
      release: result.release ?? previous.release,
      summary: result.summary ?? previous.summary,
    }
  }
  return merged
}
