/**
 * The phases, in the order they run, and how each one is invoked.
 *
 * Release logic stays in the CLI. This module builds argument vectors and reads
 * protocol documents; it does not decide how a target is built, what a release is,
 * or what gets attested. A phase that reimplemented release logic here would be a
 * second publisher with a second set of bugs, against a host that misbehaves.
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
 *
 * # Phases are the action's; operations are zup's
 *
 * A phase is a step in a workflow, and a workflow's vocabulary is its own. An
 * operation is what zup said it did, and zup's vocabulary is in
 * `zup-automation`. `operationFor` is the whole translation, and it is one table
 * rather than a naming convention — `attest` has no zup operation at all, because
 * zup does not talk to Sigstore, and a phase whose operation is `undefined` is a
 * fact the table states rather than a gap the code has to notice.
 */

import type { Inputs } from './inputs.js'
import type { Operation } from './protocol.js'

/** The workflow steps this action can run. */
export type Phase = 'build' | 'compose' | 'finalize' | 'attest' | 'publish'

/** The release manifest file name zup writes. */
export const RELEASE_MANIFEST_NAME = 'zup-release.json'

/** The signing plan file name zup writes beside the release manifest. */
export const SIGNING_PLAN_NAME = 'zup-signing.json'

/** The machine format every phase runs with. */
export const FORMAT_JSONL = 'jsonl'

/**
 * The zup operation a phase runs, or `undefined` when it runs no zup command.
 *
 * `attest` is undefined on purpose: Sigstore attestation is GitHub's OIDC token
 * exchange and Sigstore's signature format. What zup owns is which bytes are worth
 * attesting, and it says so in the release description, which the action reads
 * itself.
 */
export function operationFor(phase: Phase): Operation | undefined {
  switch (phase) {
    case 'build':
      return 'build'
    case 'compose':
      return 'publish.stage'
    case 'finalize':
      return 'sign.verify'
    case 'attest':
      return undefined
    case 'publish':
      return 'publish.github'
  }
}

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
  const operation = operationFor(phase)
  if (operation === undefined) {
    return []
  }
  return [...commandFor(phase, inputs), '--format', FORMAT_JSONL]
}

function commandFor(phase: Phase, inputs: Inputs): string[] {
  switch (phase) {
    case 'build': {
      const args = ['build']
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
    case 'compose':
      return [
        'publish',
        'stage',
        '--output',
        `${inputs.releaseDir}/web`,
        '--packages',
        `${inputs.releaseDir}/packages`,
        '--release-dir',
        inputs.releaseDir,
      ]
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
    case 'publish': {
      const args = [
        'publish',
        'github',
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
        // Only here. `zup publish stage` has no `--dry-run`: staging writes into
        // the project's own release directory, which a dry run is expected to
        // leave alone because the whole release is disposable. Passing the flag
        // anyway used to be refused by the parser, so a dry-run `release` failed
        // in its second phase.
        args.push('--dry-run')
      }
      return args
    }
    case 'attest':
      return []
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
