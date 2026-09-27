/**
 * `GITHUB_STEP_SUMMARY`, and the numbers a developer reads afterwards.
 *
 * A log is a stream; a summary is a record. Somebody looking at a release six months
 * later reads the summary and does not read the log.
 *
 * The raw JSON is deliberately absent — a summary that dumps the machine envelope is
 * why people stop opening summaries. Two tables and a status line carry everything,
 * and `release-manifest` is an output for the cases that need the document.
 */

import type { OperationResult } from './result.js'
import type { ResolvedTool } from './tool.js'

/** Everything worth remembering about one action run. */
export interface Summary {
  operation: string
  tool: ResolvedTool
  /** The zup operation this step actually performed. */
  performed: string[]
  result: OperationResult | undefined
  /** Whether the step failed, for a failure summary. */
  failure: { message: string; remedy: string } | undefined
  /** Whether the publication was a dry run. */
  dryRun: boolean
}

/** Render the summary. */
export function renderSummary(summary: Summary): string {
  if (summary.failure) {
    return failure(summary)
  }
  const lines: string[] = ['## zup', '']
  const result = summary.result

  const version = result?.appVersion
  lines.push(
    row('Version', version ?? (result ? '—' : 'not built')),
    row('Targets', result && result.targets.length > 0 ? result.targets.join(', ') : '—'),
    row('Artifacts', String(result?.artifacts.length ?? 0)),
  )
  lines.push('')
  lines.push(
    row(
      'zup CLI',
      `${summary.tool.version} (${summary.tool.identity.platform}-${summary.tool.identity.arch}, ${summary.tool.source})`,
    ),
  )
  if (result?.artifacts.length) {
    lines.push('')
    lines.push(artifactTable(result.artifacts))
  }
  if (result?.release) {
    lines.push('')
    lines.push(...releaseRows(result))
  }
  if (result?.releaseManifest !== undefined) {
    lines.push('')
    lines.push(row('Release manifest', `\`${result.releaseManifest}\``))
  }
  return `${lines.join('\n')}\n`
}

function row(label: string, value: string): string {
  return `| ${label} | ${value} |`
}

/** The artifact table. Digests are truncated: a 64-character hex defeats the column. */
function artifactTable(artifacts: OperationResult['artifacts']): string {
  const lines = ['| Artifact | Size | SHA-256 |', '| --- | ---: | --- |']
  for (const artifact of artifacts) {
    lines.push(
      `| \`${artifact.path}\` | ${formatBytes(artifact.size)} | \`${artifact.digest.slice(0, 16)}…\` |`,
    )
  }
  return lines.join('\n')
}

function releaseRows(result: OperationResult): string[] {
  const release = result.release
  if (release === undefined) {
    return []
  }
  const rows = [
    row('Release', `\`${release.tag}\``),
    row('Repository', release.repository.length > 0 ? release.repository : '—'),
    row('Status', release.state),
  ]
  if (release.immutable !== undefined) {
    rows.push(row('Immutable', release.immutable ? 'yes' : 'no'))
  }
  if (release.url !== undefined) {
    rows.push(row('URL', release.url))
  }
  if (release.assets.length > 0) {
    rows.push(row('Assets', String(release.assets.length)))
  }
  return rows
}

/** The summary a failed step produces. */
function failure(summary: Summary): string {
  return [
    '## zup',
    '',
    `**${summary.operation} failed.**`,
    '',
    summary.failure?.message ?? 'The step failed without a message.',
    '',
    summary.failure?.remedy ?? '',
    '',
    row('zup CLI', `${summary.tool.version} (${summary.tool.source})`),
  ]
    .filter((line) => line.length > 0)
    .join('\n')
    .concat('\n')
}

/** Bytes in binary units, so a summary number matches what Explorer shows. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) {
    return '—'
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
