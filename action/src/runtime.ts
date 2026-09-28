/**
 * The real implementations of every port.
 *
 * One module, so "what does this do to the machine?" has one answer readable in one
 * sitting. Everything environment-touching lives here: `process.env`,
 * `child_process`, `node:fs`, the toolkit, and the network. The rest of the action
 * depends on interfaces and is testable without any of it.
 */

import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { mkdir, readFile, rm, stat } from 'node:fs/promises'
import * as os from 'node:os'
import * as path from 'node:path'

import * as core from '@actions/core'
import * as exec from '@actions/exec'
import * as toolCache from '@actions/tool-cache'
import { RELEASE_MANIFEST_NAME } from './phases.js'
import type {
  ArtifactUpload,
  ArtifactUploadResult,
  AttestationSubject,
  CommandResult,
  CommandSpec,
  DownloadedFile,
  Downloader,
  FileSystem,
  GithubContext,
  Log,
  SourceLocation,
} from './ports.js'
import { WholeStream } from './stream.js'
import type { ReleaseAsset, ReleaseSource } from './tool.js'

/**
 * The toolkit's log, plus redaction of everything registered as secret.
 *
 * `core.setSecret` masks a value in the runner's own log but not in a workflow
 * command emitted before the masking list exists, and it is not something a test can
 * assert against. So this keeps its own list and scrubs every message it writes.
 * Defence in depth, not a replacement - and the reason the property is testable
 * without a runner.
 */
export class ToolkitLog implements Log {
  private readonly secrets: string[] = []

  private redact(message: string): string {
    return redact(message, this.secrets)
  }

  debug(message: string): void {
    core.debug(this.redact(message))
  }

  info(message: string): void {
    core.info(this.redact(message))
  }

  notice(message: string): void {
    core.notice(this.redact(message))
  }

  warning(message: string): void {
    core.warning(this.redact(message))
  }

  error(message: string): void {
    core.error(this.redact(message))
  }

  startGroup(name: string): void {
    core.startGroup(this.redact(name))
  }

  endGroup(): void {
    core.endGroup()
  }

  setSecret(value: string): void {
    this.secrets.push(value)
    core.setSecret(value)
  }

  summary(markdown: string): void {
    void core.summary.addRaw(this.redact(markdown)).write()
  }

  setOutput(name: string, value: string): void {
    core.setOutput(name, this.redact(value))
  }

  annotate(
    level: 'error' | 'warning' | 'notice',
    message: string,
    location?: SourceLocation | undefined,
  ): void {
    const properties: core.AnnotationProperties = {}
    if (location !== undefined) {
      properties.title = path.basename(location.file)
      // A property set to `undefined` is not an absent one for a workflow command:
      // `startLine=undefined` is noise in the log, and some runner versions parse it
      // as a literal string.
      if (location.startLine !== undefined) properties.startLine = location.startLine
      if (location.startColumn !== undefined) properties.startColumn = location.startColumn
      if (location.endLine !== undefined) properties.endLine = location.endLine
      if (location.endColumn !== undefined) properties.endColumn = location.endColumn
      properties.file = location.file
    }
    const text = this.redact(message)
    switch (level) {
      case 'error':
        core.error(text, properties)
        return
      case 'warning':
        core.warning(text, properties)
        return
      case 'notice':
        core.notice(text, properties)
    }
  }

  fail(message: string): void {
    core.setFailed(this.redact(message))
  }
}

/**
 * Replace every registered secret in a message.
 *
 * `split`/`join` rather than a regular expression, because a token is arbitrary
 * text that may contain pattern syntax.
 */
export function redact(message: string, secrets: readonly string[]): string {
  let result = message
  for (const secret of secrets) {
    if (secret.length === 0) {
      continue
    }
    result = result.split(secret).join('***')
  }
  return result
}

/** A process runner that never touches a shell. */
export class SpawnRunner {
  async run(spec: CommandSpec): Promise<CommandResult> {
    const framed = spec.stdout
    if (spec.stream) {
      // `getExecOutput` streams *and* captures, so one call does both. The listener
      // sees the same bytes the log does, as they arrive, which is the point: a
      // build that reports a failing check in the first second is a build somebody
      // can stop.
      const output = await exec.getExecOutput(spec.program, spec.args, {
        cwd: spec.cwd,
        env: spec.env,
        ...(spec.silent === undefined ? {} : { silent: spec.silent }),
        ...(framed === undefined
          ? {}
          : { listeners: { stdout: (data: Buffer) => framed.push(new Uint8Array(data)) } }),
        ignoreReturnCode: true,
      })
      // A stream that ended without a trailing newline still has a last line, and
      // for `--format jsonl` that line is the result the whole run was for.
      framed?.end()
      return { code: output.exitCode, stdout: output.stdout, stderr: output.stderr }
    }
    return new Promise((resolve, reject) => {
      const child = spawn(spec.program, spec.args, {
        cwd: spec.cwd,
        env: spec.env,
        // Explicit: this is the line that makes argument injection impossible. Every
        // value in `spec.args` comes from a workflow input, and a `shell: true` here
        // would hand all of them to a command interpreter.
        shell: false,
        stdio: ['ignore', 'pipe', 'pipe'],
      })
      let stderr = ''
      let stdout = ''
      const framed =
        spec.stdout ??
        new WholeStream((text) => {
          stdout = text
        })
      child.stdout.on('data', (chunk: Buffer) => {
        framed.push(new Uint8Array(chunk))
      })
      child.stderr.on('data', (chunk: Buffer) => {
        stderr += chunk.toString('utf8')
      })
      child.on('error', reject)
      child.on('close', (code) => {
        framed.end()
        // A signalled process has a null code. Reporting 1 rather than 0 is the
        // difference between "zup failed" and "zup succeeded and printed nothing".
        resolve({ code: code ?? 1, stdout, stderr })
      })
    })
  }
}

/** The real filesystem, with platform-correct path handling. */
export class NodeFileSystem implements FileSystem {
  async exists(target: string): Promise<boolean> {
    try {
      await stat(target)
      return true
    } catch {
      // A missing file is a cache miss, and so is a permissions error on a
      // self-hosted runner: the caller downloads instead either way.
      return false
    }
  }

  async readText(target: string): Promise<string> {
    return (await readFile(target, 'utf8')) as string
  }

  async readBytes(target: string): Promise<Uint8Array> {
    return new Uint8Array(await readFile(target))
  }

  async ensureDirectory(directory: string): Promise<void> {
    await mkdir(directory, { recursive: true })
  }

  async writeBytes(target: string, content: Uint8Array): Promise<void> {
    const { writeFile } = await import('node:fs/promises')
    await writeFile(target, content, { mode: 0o755 })
  }

  join(...segments: string[]): string {
    return path.join(...segments)
  }

  resolve(...segments: string[]): string {
    return path.resolve(...segments)
  }
}

/**
 * Downloads with the toolkit's own HTTP client, so a runner's proxy configuration
 * and TLS roots behave the way every other action expects.
 *
 * `contentLength` is the byte count actually on disk, not a header the server
 * claimed: the difference between the two is exactly the truncated-download case.
 */
export class ToolkitDownloader implements Downloader {
  async fetch(
    url: string,
    options: { headers?: Record<string, string>; label?: string },
  ): Promise<DownloadedFile> {
    const destination = path.join(os.tmpdir(), `zup-download-${label(url)}.bin`)
    try {
      // `downloadTool` throws rather than returning a status, and already retries
      // 5xx, 408 and 429 with backoff. Its message names the URL and status, so the
      // wrapper only adds which asset it was.
      await toolCache.downloadTool(url, destination, undefined, options.headers)
      const bytes = new Uint8Array(await readFile(destination))
      return { bytes, contentLength: bytes.byteLength }
    } catch (error) {
      throw new Error(`could not download ${options.label ?? url}: ${(error as Error).message}`)
    } finally {
      await rm(destination, { force: true }).catch(() => undefined)
    }
  }
}

/** A short, stable, filesystem-safe name for a URL. */
function label(value: string): string {
  return createHash('sha256').update(value).digest('hex').slice(0, 16)
}

/**
 * The release asset list GitHub's API returns.
 *
 * Two digest fields, and the difference matters. `digest` is GitHub's SHA-256 for
 * the uploaded asset; `release_digest` is what the release declares, which for a
 * zup release is the value zup published and the publisher verified at upload time.
 * Comparing the two catches an asset replaced after publication, and either can be
 * absent on an older release.
 */
interface ReleaseAssets {
  assets: {
    name: string
    size: number
    url: string
    digest?: string
    release_digest?: string
  }[]
}

/** The zup releases a version names, read from the release's own manifest. */
export class ZupReleaseSource implements ReleaseSource {
  private readonly cache = new Map<string, Promise<ReleaseAsset | undefined>>()

  constructor(
    private readonly repository: string,
    private readonly apiBase: string,
    private readonly version: string,
    private readonly log: Log,
  ) {}

  async asset(name: string): Promise<ReleaseAsset | undefined> {
    const existing = this.cache.get(name)
    if (existing !== undefined) {
      return existing
    }
    const pending = this.load(name)
    this.cache.set(name, pending)
    return pending
  }

  private async load(name: string): Promise<ReleaseAsset | undefined> {
    const response = await fetch(
      `${this.apiBase}/repos/${this.repository}/releases/tags/${encodeURIComponent(this.tag())}`,
      { headers: { accept: 'application/vnd.github+json', 'user-agent': 'zup-action' } },
    )
    if (!response.ok) {
      this.log.debug(
        `zup ${this.version} has no release on ${this.repository} (HTTP ${response.status})`,
      )
      return undefined
    }
    const release = (await response.json()) as ReleaseAssets
    const found = release.assets.find((entry) => entry.name === name)
    if (found === undefined) {
      return undefined
    }
    const hostDigest = stripAlgorithm(found.digest)
    const published = await this.digestFromManifest(release, name)
    if (published === undefined && hostDigest === undefined) {
      // Fail closed: a release published outside zup, or one whose manifest is
      // missing, is refused rather than trusted because the URL resolved.
      this.log.debug(
        `${name} has neither a release manifest entry nor an asset digest; refusing to use it`,
      )
      return undefined
    }
    return {
      name: found.name,
      size: found.size,
      digest: published ?? (hostDigest as string),
      hostDigest,
      url: found.url,
    }
  }

  /** The tag a version is released under. */
  private tag(): string {
    return this.version.startsWith('v') ? this.version : `v${this.version}`
  }

  /**
   * The SHA-256 zup recorded when it published, read from the release's own
   * `zup-release.json`. A CLI release is a zup release like any other, so the
   * manifest already names the digest of the executable - nothing had to be
   * invented for CI to verify it.
   */
  private async digestFromManifest(
    release: ReleaseAssets,
    name: string,
  ): Promise<string | undefined> {
    const descriptor = release.assets.find(
      (entry) => assetName(entry.name) === RELEASE_MANIFEST_NAME,
    )
    if (descriptor === undefined) {
      return undefined
    }
    try {
      const response = await fetch(descriptor.url, {
        headers: { accept: 'application/json', 'user-agent': 'zup-action' },
      })
      if (!response.ok) {
        return undefined
      }
      const manifest = (await response.json()) as {
        artifacts?: { path: string; digest: string }[]
      }
      const entry = manifest.artifacts?.find((artifact) => assetName(artifact.path) === name)
      return entry?.digest.toLowerCase()
    } catch (error) {
      this.log.debug(`could not read the release manifest: ${(error as Error).message}`)
      return undefined
    }
  }
}

/** The release asset name a manifest path becomes. */
export function assetName(manifestPath: string): string {
  return manifestPath
    .replace(/^\.\/+/u, '')
    .replaceAll('\\', '/')
    .replaceAll('/', '-')
}

function stripAlgorithm(value: string | undefined): string | undefined {
  if (value === undefined) {
    return undefined
  }
  const separator = value.indexOf(':')
  return (separator === -1 ? value : value.slice(separator + 1)).toLowerCase()
}

/**
 * Uploads to workflow artifact storage through the toolkit.
 *
 * `skipArchive` uploads a single file without a zip, which is right for a `.exe` or
 * a `.zup` package. It has one surprise worth knowing: **the service names the
 * artifact after the file**, ignoring the name that was passed. A matrix that must
 * distinguish `installer-x64` from `installer-arm64` therefore needs distinct *file*
 * names, not distinct artifact names. That is reported rather than worked around -
 * the caller gets the name the service used.
 */
export class ToolkitArtifactUploader {
  constructor(private readonly github: GithubContext) {}

  async upload(request: ArtifactUpload): Promise<ArtifactUploadResult> {
    // The artifact client carries a generated Twirp/protobuf stack of about a
    // megabyte, reachable only when a workflow sets `upload-workflow-artifacts`.
    // The dynamic import defers that cost to the workflows that ask for it.
    const { default: client } = await import('@actions/artifact')
    const response = await client.uploadArtifact(
      request.name,
      request.paths,
      request.rootDirectory,
      {
        ...(request.retentionDays === undefined ? {} : { retentionDays: request.retentionDays }),
        compressionLevel: request.direct ? 0 : 6,
        skipArchive: request.direct,
      },
    )
    return {
      id: response.id ?? 0,
      size: response.size ?? 0,
      url: this.artifactUrl(response.id),
      name: request.direct ? (request.paths[0] ?? request.name) : request.name,
    }
  }

  /**
   * The web URL for an uploaded artifact.
   *
   * The service's response carries an id and a digest and nothing a browser can
   * open. Derived from the run's own context, so a GHES run produces a GHES link.
   */
  private artifactUrl(id: number | undefined): string | undefined {
    const { repository, runId, serverUrl } = this.github
    if (id === undefined || id === 0 || repository.length === 0 || runId.length === 0) {
      return undefined
    }
    return `${serverUrl}/${repository}/actions/runs/${runId}/artifacts/${id}`
  }
}

/**
 * Creates Sigstore-backed provenance attestations.
 *
 * `actions/attest` is the current line for new implementations;
 * `attest-build-provenance` is now only a wrapper on top of it, and using the
 * package is what keeps the attestation format, the transparency-log entry and the
 * verification path correct without zup reimplementing Sigstore and OIDC. It also
 * pulls in a protobuf runtime and a noble crypto library - about two megabytes a
 * workflow which never sets `attest: true` should not pay to initialize - so the
 * import is dynamic. The OIDC token is fetched rather than supplied, because the
 * audience and the claims are the service's to choose.
 */
export class ToolkitAttestor {
  async attest(subjects: AttestationSubject[]): Promise<void> {
    if (subjects.length === 0) {
      return
    }
    const { attestProvenance } = await import('@actions/attest')
    await attestProvenance({
      subjects: subjects.map((subject) => ({
        name: subject.name,
        digest: { sha256: subject.digest },
      })),
      token: await core.getIDToken(),
    })
  }
}

/** The workflow context, as far as the action needs it. */
export function context(env: NodeJS.ProcessEnv = process.env): GithubContext {
  const serverUrl = env['GITHUB_SERVER_URL'] ?? 'https://github.com'
  let event: Record<string, unknown> = {}
  const eventPath = env['GITHUB_EVENT_PATH']
  if (eventPath !== undefined && eventPath.length > 0) {
    try {
      // Synchronous because the context is assembled before anything can be
      // awaited, and a partially-parsed event is worse than an empty one.
      event = JSON.parse(readFileSync(eventPath, 'utf8')) as Record<string, unknown>
    } catch {
      event = {}
    }
  }
  return {
    serverUrl,
    apiUrl: env['GITHUB_API_URL'] ?? `${serverUrl}/api/v3`,
    repository: env['GITHUB_REPOSITORY'] ?? '',
    runId: env['GITHUB_RUN_ID'] ?? '',
    eventName: env['GITHUB_EVENT_NAME'] ?? '',
    event,
    env: env as GithubContext['env'],
  }
}

/** Whether this run's checkout came from a fork. */
export function checkoutIsFromFork(context: GithubContext): boolean {
  const pullRequest = context.event['pull_request'] as
    | { head?: { repo?: { fork?: boolean } } }
    | undefined
  if (pullRequest?.head?.repo?.fork === true) {
    return true
  }
  // `workflow_run` carries the *triggering* workflow's event, so the head repository
  // has to be read from there.
  const workflowRun = context.event['workflow_run'] as
    | { head_repository?: { fork?: boolean }; event?: string }
    | undefined
  if (workflowRun?.head_repository?.fork === true) {
    return true
  }
  if (workflowRun?.event === 'pull_request' || workflowRun?.event === 'pull_request_target') {
    // A `workflow_run` triggered by a pull request is only safe if that pull request
    // came from a branch in this repository. Absent the head repository, refuse.
    return true
  }
  return false
}
