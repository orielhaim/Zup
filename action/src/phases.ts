/**
 * The phases, in the order they run, and how each one is invoked.
 *
 * Release logic stays in the CLI. This module builds argument vectors and reads
 * envelopes; it does not decide how a target is built, what a release is, or what
 * gets attested. A phase that reimplemented release logic here would be a second
 * publisher with a second set of bugs, against a host that misbehaves.
 *
 * ```text
 * build → compose → sign (the project's own step) → digest → attest → publish
 * ```
 *
 * Attestation before signing would attest bytes that no longer exist; publishing
 * before attesting would put out a release and then try to attach provenance to it.
 * `attest` reads the release manifest rather than globbing, because a glob over a
 * build directory finds the intermediate artifacts too, and an attestation of a file
 * nobody downloads is noise.
 */

import type { Inputs } from './inputs.js'
import type { OperationResult } from './result.js'

/** The zup operations this action can invoke. */
export type Phase = 'build' | 'compose' | 'attest' | 'publish'

/** The release manifest file name zup writes. */
export const RELEASE_MANIFEST_NAME = 'zup-release.json'

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
    case 'attest':
      return ['attest']
    case 'publish':
      return ['publish']
    case 'release':
      return ['build', 'compose', 'attest', 'publish']
    default:
      return []
  }
}

/**
 * The argument vector for one phase.
 *
 * Every value goes through as a separate argument, so a project directory with a
 * space, an ampersand or a backtick in it is just a directory.
 */
export function argumentsFor(phase: Phase, inputs: Inputs): string[] {
  switch (phase) {
    case 'build':
      return buildArguments(inputs)
    case 'compose':
      return composeArguments(inputs)
    case 'attest':
      return attestArguments(inputs)
    case 'publish':
      return publishArguments(inputs)
  }
}

function buildArguments(inputs: Inputs): string[] {
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

function composeArguments(inputs: Inputs): string[] {
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

/**
 * `attest` is not a zup subcommand.
 *
 * zup does not talk to Sigstore and should not: OIDC token exchange is GitHub's,
 * the signature format is Sigstore's, and the policy is the project's. What zup
 * owns is which bytes are worth attesting, and it says so in the release manifest.
 * The action runs zup to learn the subjects, then uses `@actions/attest`.
 */
function attestArguments(inputs: Inputs): string[] {
  void inputs
  return []
}

function publishArguments(inputs: Inputs): string[] {
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

/** The advanced arguments, appended last so a typed input can be overridden. */
export function withAdvanced(args: string[], inputs: Inputs): string[] {
  return [...args, ...inputs.args]
}

/** The release manifest path, relative to the project. */
export function releaseManifestPath(releaseDir: string): string {
  return `${releaseDir}/${RELEASE_MANIFEST_NAME}`
}

/** Whether a phase is the one that needs the publish credential. */
export function needsToken(phase: Phase): boolean {
  return phase === 'publish'
}

/** Whether a phase produces a release directory worth uploading. */
export function producesArtifacts(phase: Phase): boolean {
  return phase === 'build' || phase === 'compose'
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
    merged =
      merged === undefined
        ? { ...result, operation: 'release' }
        : {
            ...result,
            operation: 'release',
            success: merged.success && result.success,
            targets: [...new Set([...merged.targets, ...result.targets])],
            artifacts: [...merged.artifacts, ...result.artifacts],
            diagnostics: [...merged.diagnostics, ...result.diagnostics],
            releaseManifest: result.releaseManifest ?? merged.releaseManifest,
            appVersion: result.appVersion ?? merged.appVersion,
            release: result.release ?? merged.release,
            summary: result.summary ?? merged.summary,
          }
  }
  return merged
}
