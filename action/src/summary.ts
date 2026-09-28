/**
 * `GITHUB_STEP_SUMMARY`, and the numbers a developer reads afterwards.
 *
 * A log is a stream; a summary is a record. Somebody looking at a release six months
 * later reads the summary and does not read the log.
 *
 * The raw JSON is deliberately absent - a summary that dumps the machine document is
 * why people stop opening summaries. Two tables and a status line carry everything,
 * and `release-manifest` is an output for the cases that need the document.
 */

import type { Phase } from './phases.js'
import { type Artifact, type AutomationResult, isSigned } from './protocol.js'
import type { ResolvedTool } from './tool.js'

/** Everything worth remembering about one action run. */
export interface Summary {
  operation: string
  tool: ResolvedTool
  /** The workflow steps this step ran. */
  performed: Phase[]
  /** One zup result per phase, in the order they ran. */
  results: Map<Phase, AutomationResult>
  /** The one result the summary is rendered from, or `undefined` if none ran. */
  result: AutomationResult | undefined
  /** Whether the step failed, for a failure summary. */
  failure: { message: string; remedy: string } | undefined
  /** Whether the publication was a dry run. */
  dryRun: boolean
}

/**
 * Fold every phase's result into the one the outputs and the summary report.
 *
 * Not a synthetic document: the *fields* are the union of what the phases said, and
 * the summary never claims a phase's operation was another phase's. `status` is the
 * conjunction, because a run that composed and then failed to publish did not
 * succeed.
 *
 * `operation` is `undefined` rather than a made-up name. zup's operation vocabulary
 * has no word for "a workflow's five steps", and a result that claimed one would be
 * a name in the protocol that no zup ever emits.
 */
export function mergeResults(
  phases: readonly Phase[],
  results: ReadonlyMap<Phase, AutomationResult>,
): AutomationResult | undefined {
  let merged: AutomationResult | undefined
  for (const phase of phases) {
    const result = results.get(phase)
    if (result === undefined) {
      continue
    }
    if (merged === undefined) {
      merged = { ...result, operation: '' }
      continue
    }
    // Bound to a `const` so the narrowing survives into the callbacks below: a
    // `let` is not narrowed inside a closure, and the artifact merge is two
    // closures.
    const previous = merged
    merged = {
      ...previous,
      operation: '',
      status: previous.status === 'success' ? result.status : 'failure',
      application: result.application ?? previous.application,
      targets: dedupe([...previous.targets, ...result.targets], (target) => target.profile),
      artifacts: mergeArtifacts(previous.artifacts, result.artifacts),
      release_manifest: result.release_manifest ?? previous.release_manifest,
      publication: result.publication ?? previous.publication,
      diagnostics: dedupe(
        [...previous.diagnostics, ...result.diagnostics],
        (diagnostic) => `${diagnostic.code} ${diagnostic.message}`,
      ),
      summary: result.summary ?? previous.summary,
    }
  }
  return merged
}

function dedupe<T>(values: readonly T[], key: (value: T) => string): T[] {
  const seen = new Map<string, T>()
  for (const value of values) {
    const identity = key(value)
    if (!seen.has(identity)) {
      seen.set(identity, value)
    }
  }
  return [...seen.values()]
}

/**
 * Artifacts from two phases, keyed by path.
 *
 * The later value wins, and it has to: `sign verify` rewrites the release
 * description with the *published* digests, and a summary that showed the
 * pre-signature ones would show bytes no downloader receives.
 */
function mergeArtifacts(left: readonly Artifact[], right: readonly Artifact[]): Artifact[] {
  const byPath = new Map<string, Artifact>()
  for (const artifact of left) {
    byPath.set(artifact.path, artifact)
  }
  for (const artifact of right) {
    byPath.set(artifact.path, artifact)
  }
  return [...byPath.values()]
}

/** Render the summary. */
export function renderSummary(summary: Summary): string {
  if (summary.failure) {
    return failure(summary)
  }
  const lines: string[] = ['## zup', '']
  const result = summary.result
  const application = result?.application

  lines.push(
    row('Version', application?.version ?? (result === undefined ? 'not built' : '-')),
    row(
      'Targets',
      result !== undefined && result.targets.length > 0
        ? result.targets.map((target) => target.profile).join(', ')
        : '-',
    ),
    row('Artifacts', String(result?.artifacts.length ?? 0)),
  )
  lines.push('')
  lines.push(
    row(
      'zup CLI',
      `${summary.tool.version} (${summary.tool.identity.platform}-${summary.tool.identity.arch}, ${summary.tool.source})`,
    ),
  )
  if (result !== undefined && result.artifacts.length > 0) {
    lines.push('')
    lines.push(artifactTable(result.artifacts))
  }
  if (result?.publication != null) {
    lines.push('')
    lines.push(...publicationRows(result.publication))
  }
  if (result?.release_manifest != null) {
    lines.push('')
    lines.push(row('Release manifest', `\`${result.release_manifest}\``))
  }
  return `${lines.join('\n')}\n`
}

function row(label: string, value: string): string {
  return `| ${label} | ${value} |`
}

/** The artifact table. Digests are truncated: a 64-character hex defeats the column. */
function artifactTable(artifacts: readonly Artifact[]): string {
  const lines = ['| Artifact | Size | Signed | SHA-256 |', '| --- | ---: | :---: | --- |']
  for (const artifact of artifacts) {
    lines.push(
      `| \`${artifact.path}\` | ${formatBytes(artifact.size)} | ${isSigned(artifact) ? '✓' : '-'} | \`${artifact.digest.value.slice(0, 16)}…\` |`,
    )
  }
  return lines.join('\n')
}

function publicationRows(publication: NonNullable<AutomationResult['publication']>): string[] {
  const rows = [
    row('Release', `\`${publication.tag}\``),
    row('Repository', publication.subject.length > 0 ? publication.subject : '-'),
    row('Status', publication.state),
  ]
  if (publication.immutable !== null) {
    rows.push(row('Immutable', publication.immutable ? 'yes' : 'no'))
  }
  if (publication.url !== null) {
    rows.push(row('URL', publication.url))
  }
  if (publication.assets.length > 0) {
    rows.push(row('Assets', String(publication.assets.length)))
  }
  return rows
}

/**
 * The summary a failed step produces.
 *
 * The artifacts and the release manifest are shown even on failure, because a
 * build that produced four installers and then failed to sign them is the case a
 * developer most needs a record of.
 */
function failure(summary: Summary): string {
  const result = summary.result
  const lines = [
    '## zup',
    '',
    `**${summary.operation} failed.**`,
    '',
    summary.failure?.message ?? 'The step failed without a message.',
  ]
  if (summary.failure?.remedy) {
    lines.push('', summary.failure.remedy)
  }
  if (result !== undefined && result.artifacts.length > 0) {
    lines.push('', artifactTable(result.artifacts))
  }
  if (result?.release_manifest != null) {
    lines.push('', row('Release manifest', `\`${result.release_manifest}\``))
  }
  lines.push('', row('zup CLI', `${summary.tool.version} (${summary.tool.source})`))
  return `${lines.join('\n')}\n`
}

/** Bytes in binary units, so a summary number matches what Explorer shows. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) {
    return '-'
  }
  if (bytes < 1024) {
    return `${bytes} B`
  }
  const units = ['KiB', 'MiB', 'GiB', 'TiB']
  let value = bytes / 1024
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  const rendered = value >= 100 ? value.toFixed(0) : value.toFixed(1)
  return `${rendered} ${units[unit]}`
}
