/**
 * Every effect the action has, as a type.
 *
 * `runtime.ts` binds these to the GitHub Toolkit and Node; the tests bind them to
 * records. That substitution is what lets the secret-isolation tests assert a
 * property rather than assert that a string is absent from a log line.
 */

import type { RunnerEnvironment } from './platform.js'

/** A command to run, with no shell anywhere in the picture. */
export interface CommandSpec {
  /** The executable, resolved or absolute. */
  program: string
  /** Arguments, already tokenized. Never a command string. */
  args: string[]
  cwd: string
  /**
   * The child's complete environment: a replacement, not an addition. A child that
   * inherits the parent's environment inherits `GITHUB_TOKEN` whether or not that
   * was intended, so every environment here is built from scratch.
   */
  env: Record<string, string>
  /** Stream the child's output to the workflow log. */
  stream?: boolean
  /** Values to replace in the log with `***`. */
  silent?: boolean
}

/** What a finished process left behind. */
export interface CommandResult {
  code: number
  stdout: string
  stderr: string
}

export interface ProcessRunner {
  run(spec: CommandSpec): Promise<CommandResult>
  which(program: string): Promise<string | undefined>
}

/**
 * Reads a release asset.
 *
 * The expected size and digest are parameters rather than something the
 * implementation looks up afterwards: a downloader that cannot be told what to
 * expect is a downloader with no verification.
 */
export interface Downloader {
  fetch(
    url: string,
    options: { headers?: Record<string, string>; label?: string },
  ): Promise<DownloadedFile>
}

/** Bytes that arrived, and the `Content-Length` the server reported, if any. */
export interface DownloadedFile {
  bytes: Uint8Array
  contentLength: number | undefined
}

export interface FileSystem {
  exists(path: string): Promise<boolean>
  readText(path: string): Promise<string>
  /** Raw bytes. Separate from `readText` because verification hashes them. */
  readBytes(path: string): Promise<Uint8Array>
  isDirectory(path: string): Promise<boolean>
  listFiles(directory: string): Promise<string[]>
  ensureDirectory(directory: string): Promise<void>
  /** Write bytes, creating parents. Fails rather than truncating silently. */
  writeBytes(path: string, content: Uint8Array): Promise<void>
  join(...segments: string[]): string
  resolve(...segments: string[]): string
}

/** Where the action writes what a user reads. */
export interface Log {
  debug(message: string): void
  info(message: string): void
  notice(message: string): void
  warning(message: string): void
  error(message: string): void
  startGroup(name: string): void
  endGroup(): void
  /** Register a value the runner must mask everywhere it appears. */
  setSecret(value: string): void
  summary(markdown: string): void
  setOutput(name: string, value: string): void
  annotate(
    level: 'error' | 'warning' | 'notice',
    message: string,
    location?: SourceLocation | undefined,
  ): void
  /** Mark the step as failed with a message the developer can act on. */
  fail(message: string): void
}

/** A source region, in the shape the runner's workflow commands take. */
export interface SourceLocation {
  file: string
  startLine?: number | undefined
  startColumn?: number | undefined
  endLine?: number | undefined
  endColumn?: number | undefined
}

/** The workflow context, as far as the action needs it. */
export interface GithubContext {
  readonly serverUrl: string
  readonly apiUrl: string
  readonly repository: string
  readonly workflow: string
  readonly runId: string
  readonly eventName: string
  readonly ref: string
  readonly actor: string
  /** The event payload, for the checks that need it. */
  readonly event: Record<string, unknown>
  /** The environment the runner provided. */
  readonly env: RunnerEnvironment & Record<string, string | undefined>
  /** Whether the runner is in debug mode. */
  readonly isDebug: boolean
}

export interface ArtifactUploader {
  upload(request: ArtifactUpload): Promise<ArtifactUploadResult>
}

export interface ArtifactUpload {
  /**
   * Requested artifact name. A direct upload ignores it: the service names an
   * unarchived artifact after the file it contains, and reports that in the result.
   */
  name: string
  paths: string[]
  /** The directory paths are relative to, and which the archive is rooted at. */
  rootDirectory: string
  retentionDays: number | undefined
  /**
   * Whether the payload is a single already-compressed file. zup's outputs mostly
   * do not compress — a `.exe`, a `.zup` transport package, a `.tar.zst` — so
   * zipping them spends CPU and storage to make the file bigger.
   */
  direct: boolean
}

export interface ArtifactUploadResult {
  id: number
  size: number
  url: string | undefined
  /** The name the service recorded, which on a direct upload is the file's own. */
  name: string
}

/** One file to attest, and the digest it is attested at. */
export interface AttestationSubject {
  name: string
  /** Lowercase hex SHA-256, as the release manifest recorded it. */
  digest: string
}

export interface Attestor {
  attest(subjects: AttestationSubject[]): Promise<void>
}
