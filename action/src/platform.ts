/**
 * Which runner is this, and which zup release asset does that mean.
 *
 * Runner and target are different axes. A Windows x64 runner that cross-compiles an
 * aarch64 Linux installer still needs a `windows-x64` zup, because zup is what does the
 * compiling. This module maps runner identity to tool identity and never consults the
 * project's target matrix.
 */

/** A runner platform, normalized. */
export type RunnerPlatform = 'windows' | 'linux' | 'macos'

/** A CPU architecture, normalized. */
export type RunnerArch = 'x64' | 'arm64'

/** A runner, as a zup release asset name fragment. */
export interface RunnerIdentity {
  platform: RunnerPlatform
  arch: RunnerArch
}

/** The zup release asset for one runner. */
export interface ToolAsset {
  /** The asset name in the zup release, e.g. `zup-windows-x64.exe`. */
  name: string
  /** The file name the tool is cached under. */
  file: string
  /** Whether the file is executable as written. */
  executable: boolean
}

/** The raw environment values a runner reports. */
export interface RunnerEnvironment {
  RUNNER_OS?: string | undefined
  RUNNER_ARCH?: string | undefined
}

/** Why a runner cannot run zup. A type, so the refusals are enumerable. */
export class UnsupportedRunnerError extends Error {
  constructor(
    readonly runnerOs: string,
    readonly runnerArch: string,
    readonly supported: readonly string[],
  ) {
    super(
      `zup has no release for ${runnerOs}/${runnerArch}. ` +
        `Supported runners: ${supported.join(', ')}. ` +
        'On a self-hosted runner, build zup from source and pass `zup-path`, ' +
        'or run this job on a hosted runner.',
    )
    this.name = 'UnsupportedRunnerError'
  }
}

/** Every platform/architecture pair zup publishes a release asset for. */
export const SUPPORTED_RUNNERS: readonly string[] = [
  'windows/x64',
  'windows/arm64',
  'linux/x64',
  'linux/arm64',
  'macos/x64',
  'macos/arm64',
]

/** Normalize `RUNNER_OS`. GitHub reports `macOS`; an asset is named `macos`. */
export function normalizePlatform(value: string | undefined): RunnerPlatform | undefined {
  switch (value?.trim().toLowerCase()) {
    case 'windows':
      return 'windows'
    case 'linux':
      return 'linux'
    case 'macos':
      return 'macos'
    default:
      return undefined
  }
}

/**
 * Normalize `RUNNER_ARCH`.
 *
 * `X64` is what GitHub reports and `x86_64` is what a Linux `uname` reports for the
 * same machine. Treating them as different runners is how a tool cache grows a `-1`
 * suffix and then misses forever.
 */
export function normalizeArch(value: string | undefined): RunnerArch | undefined {
  switch (value?.trim().toLowerCase()) {
    case 'x64':
    case 'x86_64':
    case 'amd64':
      return 'x64'
    case 'arm64':
    case 'aarch64':
      return 'arm64'
    default:
      return undefined
  }
}

/** The runner this process is on, or a refusal naming what is missing. */
export function identifyRunner(environ: RunnerEnvironment): RunnerIdentity {
  const rawOs = environ.RUNNER_OS ?? '(unset)'
  const rawArch = environ.RUNNER_ARCH ?? '(unset)'
  const platform = normalizePlatform(environ.RUNNER_OS)
  const arch = normalizeArch(environ.RUNNER_ARCH)
  if (!platform || !arch) {
    throw new UnsupportedRunnerError(rawOs, rawArch, SUPPORTED_RUNNERS)
  }
  return { platform, arch }
}

/**
 * The zup release asset for one runner.
 *
 * The name is the release contract rather than a preference: `zup publish github`
 * uploads `zup-<platform>-<arch>` for the CLI and this resolves against that same name.
 * The `.exe` matters too — a `zup` without it on a Windows runner is a file the shell
 * will not run and the cache will happily store.
 */
export function toolAsset(identity: RunnerIdentity): ToolAsset {
  const base = `zup-${identity.platform}-${identity.arch}`
  const windows = identity.platform === 'windows'
  const file = windows ? `${base}.exe` : base
  return { name: file, file, executable: !windows }
}

/** The cache key for one tool version and runner. */
export function toolCacheVersion(version: string, identity: RunnerIdentity): string {
  return `${version}-${identity.platform}-${identity.arch}`
}
