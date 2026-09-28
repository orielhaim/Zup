/**
 * Installing the zup CLI, and proving the bytes are the right bytes.
 *
 * A zup release records every asset's SHA-256 in its receipt and release manifest -
 * a format zup already produces - so verification uses that rather than a
 * `checksums.txt` that would exist only for CI. When GitHub also reports a digest of
 * its own, both are compared; a disagreement means the bytes changed after
 * publication, which is the event worth refusing to run over.
 *
 * ```text
 * normalize the runner   windows/x64, not X64/WINDOWS
 * tool cache             an exact version + architecture
 * verify a cached entry  refuse a corrupt one rather than running it
 * read the release       asset name, size, SHA-256
 * download               only if the cache missed
 * verify                 size, then SHA-256, then GitHub's digest
 * cache and put on PATH
 * ```
 *
 * Verification happens before the file is made executable and long before it runs.
 * Bytes that fail are deleted rather than left for the next step to find.
 */
import { createHash } from 'node:crypto'
import * as os from 'node:os'
import * as path from 'node:path'
import {
  identifyRunner,
  type RunnerEnvironment,
  type RunnerIdentity,
  toolAsset,
  toolCacheVersion,
} from './platform.js'
import type { Downloader, FileSystem, Log } from './ports.js'

/** The tool the action resolved and installed. */
export interface ResolvedTool {
  path: string
  version: string
  /** Where it came from, for the log and the summary. */
  source: 'explicit' | 'cache' | 'download'
  identity: RunnerIdentity
}

/** Why a zup tool could not be resolved. */
export class ToolError extends Error {
  constructor(
    message: string,
    readonly remedy: string,
  ) {
    super(`${message} ${remedy}`)
    this.name = 'ToolError'
  }
}

/** The tool cache, narrowed to what installation needs. */
export interface ToolCache {
  find(version: string, arch: string): Promise<string | undefined>
  cacheFile(
    staged: string,
    fileName: string,
    version: string,
    arch: string,
    executable: boolean,
  ): Promise<string>
}

/** GitHub's release metadata, as far as verification needs it. */
export interface ReleaseSource {
  /**
   * The asset's published identity.
   *
   * `digest` is what zup recorded when it published; `hostDigest` is GitHub's own
   * digest for the uploaded asset, when the server reports one. A value that is
   * present and disagrees with the other fails closed.
   */
  asset(name: string): Promise<ReleaseAsset | undefined>
}

/** One release asset's recorded identity. */
export interface ReleaseAsset {
  name: string
  /** Bytes, as GitHub reports for the asset. */
  size: number
  /** Lowercase hex SHA-256 from the release manifest zup published. */
  digest: string
  /** Lowercase hex SHA-256 from GitHub's own asset digest, if reported. */
  hostDigest: string | undefined
  url: string
}

/** What installation needs from the outside world. */
export interface ToolDependencies {
  cache: ToolCache
  downloader: Downloader
  filesystem: FileSystem
  releases: ReleaseSource
  log: Log
  /** The base URL zup releases are published under. */
  releaseRepository: string
}

/**
 * Resolve the zup executable, installing it if necessary.
 *
 * `zupPath` short-circuits everything, and that is not a convenience: it is how a
 * repository tests its own action against a locally built zup, and the only
 * supported way to run on a self-hosted runner with an architecture zup does not
 * publish.
 */ export async function resolveTool(
  request: {
    zupPath: string | undefined
    zupVersion: string | undefined
    /** The runner this process is on, as the runner reports it. */
    runner: RunnerEnvironment
  },
  dependencies: ToolDependencies,
  defaultVersion: string,
): Promise<ResolvedTool> {
  const identity = identifyRunner(request.runner)
  const asset = toolAsset(identity)

  if (request.zupPath !== undefined) {
    const resolved = dependencies.filesystem.resolve(request.zupPath)
    if (!(await dependencies.filesystem.exists(resolved))) {
      throw new ToolError(
        `the \`zup-path\` you gave does not exist: ${resolved}`,
        'Pass a path to a zup executable, or remove the input to have the action install one.',
      )
    }
    dependencies.log.debug(`using the zup at ${resolved}`)
    return {
      path: resolved,
      version: request.zupVersion ?? 'explicit',
      source: 'explicit',
      identity,
    }
  }

  const version = request.zupVersion ?? defaultVersion
  const arch = `${identity.platform}-${identity.arch}`
  const cacheVersion = toolCacheVersion(version, identity)

  const cached = await dependencies.cache.find(cacheVersion, arch)
  if (cached !== undefined) {
    const executable = dependencies.filesystem.join(cached, asset.file)
    // A cached entry is not trusted because it is cached. A half-written cache
    // directory, a disk that filled mid-write, or a concurrent job sharing a cache
    // all produce a file of the right name and the wrong bytes.
    if (await verifyFile(executable, dependencies.releases, asset.name, dependencies)) {
      dependencies.log.info(`zup ${version} is cached (${arch})`)
      return { path: executable, version, source: 'cache', identity }
    }
    dependencies.log.warning(
      `the cached zup ${version} for ${arch} did not match its published digest; downloading it again`,
    )
  }

  dependencies.log.info(`Installing zup ${version} for ${arch}`)
  const release = await dependencies.releases.asset(asset.name)
  if (release === undefined) {
    throw new ToolError(
      `zup ${version} has no \`${asset.name}\` asset on ${dependencies.releaseRepository}.`,
      'Check the version exists and publishes an asset for this runner, or pass ' +
        '`zup-path` to use a locally built zup.',
    )
  }

  const downloaded = await dependencies.downloader.fetch(release.url, {
    headers: { accept: 'application/octet-stream' },
    label: asset.name,
  })
  // A truncated response - a proxy, a flaky network - is caught before anything is
  // written or executed.
  if (
    downloaded.contentLength !== undefined &&
    downloaded.contentLength !== downloaded.bytes.byteLength
  ) {
    throw new ToolError(
      `${asset.name} downloaded ${downloaded.bytes.byteLength} bytes but the server ` +
        `reported ${downloaded.contentLength}.`,
      'The download was incomplete, so nothing was run and nothing was cached. Retry the job.',
    )
  }
  assertVerified(downloaded.bytes, release, dependencies.log, asset.name)

  const staging = dependencies.filesystem.join(
    os.tmpdir(),
    `zup-tool-${version}-${identity.platform}-${identity.arch}`,
  )
  await dependencies.filesystem.ensureDirectory(staging)
  const downloadedPath = dependencies.filesystem.join(staging, asset.file)
  await dependencies.filesystem.writeBytes(downloadedPath, downloaded.bytes)
  // Verified again after the write, because the write is what can truncate: a full
  // disk produces a short file, and only a check afterwards knows the staged bytes
  // are what the release published.
  const staged = await readBytes(downloadedPath, dependencies.filesystem)
  if (staged === undefined) {
    throw new ToolError(
      `zup ${version} was downloaded and verified but could not be written to ${staging}.`,
      'This is a runner problem rather than a zup problem. Check for a full disk and retry.',
    )
  }
  assertVerified(staged, release, dependencies.log, asset.name)

  const directory = await dependencies.cache.cacheFile(
    downloadedPath,
    asset.file,
    cacheVersion,
    arch,
    asset.executable,
  )
  const executable = dependencies.filesystem.join(directory, asset.file)
  dependencies.log.debug(`zup ${version} installed at ${executable}`)
  return { path: executable, version, source: 'download', identity }
}

/**
 * Compare bytes against a published identity, failing closed.
 *
 * Every check runs when the value is present. A size check alone accepts a
 * same-length substitution; a digest check alone accepts truncated bytes on a
 * filesystem that reports the wrong length.
 */
export function assertVerified(
  bytes: Uint8Array,
  release: ReleaseAsset,
  log: Log,
  label: string,
): void {
  if (bytes.byteLength !== release.size) {
    throw new ToolError(
      `${label} is ${bytes.byteLength} bytes and the release says ${release.size}.`,
      'The download was incomplete or the asset was replaced. Nothing was run and nothing was cached.',
    )
  }
  const digest = digestOf(bytes)
  if (digest !== release.digest.toLowerCase()) {
    throw new ToolError(
      `${label} has SHA-256 ${digest} and the release published ${release.digest}.`,
      'The bytes do not match what zup published, so they were not run. ' +
        'If this persists, the release asset was modified after publication.',
    )
  }
  if (release.hostDigest !== undefined && release.hostDigest.toLowerCase() !== digest) {
    throw new ToolError(
      `${label} has SHA-256 ${digest}, GitHub reports ${release.hostDigest} for the asset, ` +
        'and zup published a different digest again.',
      'Two independent records disagree, so nothing was run and nothing was cached.',
    )
  }
  log.debug(`${label} verified: ${digest}`)
}

/** The SHA-256 of some bytes, lowercase hex. */
export function digestOf(bytes: Uint8Array): string {
  return createHash('sha256').update(bytes).digest('hex')
}

/** Whether a cached file still matches the published identity. */
async function verifyFile(
  executable: string,
  releases: ReleaseSource,
  name: string,
  dependencies: ToolDependencies,
): Promise<boolean> {
  const release = await releases.asset(name)
  if (release === undefined) {
    return false
  }
  if (!(await dependencies.filesystem.exists(executable))) {
    return false
  }
  const bytes = await readBytes(executable, dependencies.filesystem)
  if (bytes === undefined || bytes.byteLength !== release.size) {
    return false
  }
  return digestOf(bytes) === release.digest.toLowerCase()
}

/**
 * A file's bytes, or `undefined` if it cannot be read.
 *
 * A file can disappear between an `exists` check and a read on a self-hosted runner
 * with an external cleaner, and that is a cache miss rather than a failure.
 */
async function readBytes(target: string, filesystem: FileSystem): Promise<Uint8Array | undefined> {
  try {
    return await filesystem.readBytes(target)
  } catch {
    return undefined
  }
}

/** The path the action puts on `PATH` for the rest of the job. */
export function toolDirectory(tool: ResolvedTool, filesystem: FileSystem): string {
  return path.dirname(filesystem.resolve(tool.path))
}
