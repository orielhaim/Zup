import { describe, expect, it } from 'bun:test'
import {
  identifyRunner,
  normalizeArch,
  normalizePlatform,
  SUPPORTED_RUNNERS,
  toolAsset,
  toolCacheVersion,
  UnsupportedRunnerError,
} from '../src/platform.js'
import type { DownloadedFile, Downloader, FileSystem } from '../src/ports.js'
import {
  assertVerified,
  digestOf,
  type ReleaseAsset,
  type ReleaseSource,
  resolveTool,
  type ToolCache,
  ToolError,
} from '../src/tool.js'
import { silentLog } from './fixtures.js'

/**
 * A tool cache backed by the same filesystem the action reads through.
 *
 * Backed rather than recorded on purpose: the read-back-after-write check exists
 * precisely because writing and reading are different operations, and a fake that
 * remembers writes without going through a read cannot test it.
 */
class FakeCache implements ToolCache {
  readonly stored = new Map<string, string>()

  constructor(private readonly filesystem: FakeFileSystem) {}

  async find(version: string): Promise<string | undefined> {
    return this.stored.get(version)
  }

  async cacheFile(staged: string, fileName: string, version: string): Promise<string> {
    const bytes = await this.filesystem.readBytes(staged)
    const directory = `/cache/${version}`
    this.filesystem.add(`${directory}/${fileName}`, bytes)
    this.stored.set(version, directory)
    return directory
  }
}

/** A filesystem that is a map, with Windows-style or POSIX-style paths. */
class FakeFileSystem implements FileSystem {
  private readonly bytes = new Map<string, Uint8Array>()

  constructor(
    files: Record<string, string | Uint8Array> = {},
    private readonly windows = false,
  ) {
    for (const [name, contents] of Object.entries(files)) {
      this.add(name, contents)
    }
  }

  async exists(target: string): Promise<boolean> {
    return this.bytes.has(this.normalize(target))
  }

  async readText(target: string): Promise<string> {
    const found = this.bytes.get(this.normalize(target))
    if (found === undefined) {
      throw new Error(`no such file: ${target}`)
    }
    return new TextDecoder().decode(found)
  }

  async readBytes(target: string): Promise<Uint8Array> {
    const found = this.bytes.get(this.normalize(target))
    if (found === undefined) {
      throw new Error(`no such file: ${target}`)
    }
    return found
  }

  async ensureDirectory(): Promise<void> {}

  async writeBytes(target: string, content: Uint8Array): Promise<void> {
    this.add(target, content)
  }

  join(...segments: string[]): string {
    return segments.join(this.windows ? '\\' : '/')
  }

  resolve(...segments: string[]): string {
    return this.normalize(segments.join(this.windows ? '\\' : '/'))
  }

  add(target: string, contents: string | Uint8Array): void {
    const bytes = typeof contents === 'string' ? new TextEncoder().encode(contents) : contents
    this.bytes.set(this.normalize(target), bytes)
  }

  /** Everything the fake holds, so a test can assert that nothing was cached. */
  entries(): [string, Uint8Array][] {
    return [...this.bytes.entries()]
  }

  private normalize(target: string): string {
    return this.windows ? target.replaceAll('/', '\\') : target
  }
}

/**
 * A downloader that returns a fixed payload, and counts its calls.
 *
 * `reported` is what the server claims the payload's length is, which is not
 * necessarily how many bytes actually arrived.
 */
class FakeDownloader implements Downloader {
  calls = 0

  constructor(
    private readonly bytes: Uint8Array,
    private readonly reported?: number,
  ) {}

  async fetch(): Promise<DownloadedFile> {
    this.calls += 1
    return { bytes: this.bytes, contentLength: this.reported ?? this.bytes.byteLength }
  }
}

/** A release that publishes one asset with a known identity. */
class FakeRelease implements ReleaseSource {
  constructor(private asset_: ReleaseAsset | undefined) {}

  async asset(): Promise<ReleaseAsset | undefined> {
    return this.asset_
  }
}

const PAYLOAD = new Uint8Array([0x7f, 0x45, 0x4c, 0x46, 0x02, 0x01])
const PAYLOAD_DIGEST = digestOf(PAYLOAD)

const LINUX_ASSET: ReleaseAsset = {
  name: 'zup-linux-x64',
  size: PAYLOAD.byteLength,
  digest: PAYLOAD_DIGEST,
  hostDigest: undefined,
  url: 'https://example.test/zup-linux-x64',
}

/**
 * The runner this test pretends to be on.
 *
 * Passed in rather than set on `process.env`, so the tool resolver has no ambient
 * environment to read - which is what makes every case here reachable without a
 * runner.
 */
const LINUX_X64 = { RUNNER_OS: 'linux', RUNNER_ARCH: 'X64' }
const FREEBSD_X64 = { RUNNER_OS: 'freebsd', RUNNER_ARCH: 'x64' }

/** One resolution, with only the parts a case is about left to vary. */
function resolve(
  overrides: {
    zupPath?: string
    zupVersion?: string
    runner?: typeof LINUX_X64
    /** `null` for a release that does not publish the asset at all. */
    asset?: ReleaseAsset | null
    files?: Record<string, string | Uint8Array>
    /** The cache key to seed a cache hit under. */
    cached?: string
    payload?: Uint8Array
    /** The length the server claims, when it does not match the bytes. */
    contentLength?: number
  } = {},
) {
  const filesystem = new FakeFileSystem(overrides.files ?? {})
  const cached = new FakeCache(filesystem)
  if (overrides.cached !== undefined) {
    cached.stored.set(overrides.cached, '/cache/1.4.0-linux-x64')
  }
  const downloader = new FakeDownloader(overrides.payload ?? PAYLOAD, overrides.contentLength)
  const asset = overrides.asset === undefined ? LINUX_ASSET : overrides.asset
  const call = resolveTool(
    {
      zupPath: overrides.zupPath,
      zupVersion: overrides.zupVersion,
      runner: overrides.runner ?? LINUX_X64,
    },
    {
      cache: cached,
      downloader,
      filesystem,
      releases: new FakeRelease(asset ?? undefined),
      log: silentLog(),
      releaseRepository: 'acme/zup',
    },
    '0.0.1',
  )
  return { call, filesystem, downloader }
}

describe('runner identity', () => {
  it('normalizes the platform spellings GitHub uses', () => {
    expect(normalizePlatform('Windows')).toBe('windows')
    expect(normalizePlatform('macOS')).toBe('macos')
    expect(normalizePlatform('linux')).toBe('linux')
    expect(normalizePlatform('solaris')).toBeUndefined()
    expect(normalizePlatform(undefined)).toBeUndefined()
  })

  it('normalizes both spellings of an architecture', () => {
    // GitHub says `X64`; `uname` says `x86_64`. Treating them as two runners is
    // how a tool cache gets a `-1` suffix and misses forever.
    expect(normalizeArch('X64')).toBe('x64')
    expect(normalizeArch('x86_64')).toBe('x64')
    expect(normalizeArch('amd64')).toBe('x64')
    expect(normalizeArch('ARM64')).toBe('arm64')
    expect(normalizeArch('aarch64')).toBe('arm64')
    expect(normalizeArch('ppc64le')).toBeUndefined()
  })

  it('names an asset per platform and architecture', () => {
    expect(toolAsset({ platform: 'windows', arch: 'x64' }).name).toBe('zup-windows-x64.exe')
    expect(toolAsset({ platform: 'windows', arch: 'arm64' }).name).toBe('zup-windows-arm64.exe')
    expect(toolAsset({ platform: 'linux', arch: 'x64' }).name).toBe('zup-linux-x64')
    expect(toolAsset({ platform: 'linux', arch: 'arm64' }).name).toBe('zup-linux-arm64')
    expect(toolAsset({ platform: 'macos', arch: 'x64' }).name).toBe('zup-macos-x64')
    expect(toolAsset({ platform: 'macos', arch: 'arm64' }).name).toBe('zup-macos-arm64')
  })

  it('appends .exe only on windows', () => {
    expect(toolAsset({ platform: 'windows', arch: 'x64' }).executable).toBe(false)
    expect(toolAsset({ platform: 'linux', arch: 'x64' }).executable).toBe(true)
  })

  it('keys the cache by version and architecture, not by version alone', () => {
    expect(toolCacheVersion('1.4.0', { platform: 'linux', arch: 'x64' })).toBe('1.4.0-linux-x64')
    expect(toolCacheVersion('1.4.0', { platform: 'linux', arch: 'arm64' })).toBe(
      '1.4.0-linux-arm64',
    )
  })

  it('covers every runner zup publishes for', () => {
    for (const pair of SUPPORTED_RUNNERS) {
      const [platform, arch] = pair.split('/') as [string, string]
      expect(toolAsset({ platform: platform as never, arch: arch as never }).name).toContain(
        pair.replace('/', '-'),
      )
    }
  })

  it('refuses an unsupported runner with a message naming the alternatives', () => {
    expect(() => identifyRunner({ RUNNER_OS: 'solaris', RUNNER_ARCH: 'x64' })).toThrow(
      UnsupportedRunnerError,
    )
    try {
      identifyRunner({ RUNNER_OS: 'freebsd', RUNNER_ARCH: 'riscv64' })
      expect.unreachable('an unsupported runner must be refused')
    } catch (error) {
      const message = (error as Error).message
      expect(message).toContain('freebsd/riscv64')
      expect(message).toContain('windows/x64')
      expect(message).toContain('zup-path')
    }
  })

  it('refuses a missing RUNNER_ARCH rather than assuming x64', () => {
    // Assuming x64 would install a binary the runner cannot execute, and the failure
    // would surface as a linker error from zup rather than from here.
    expect(() => identifyRunner({ RUNNER_OS: 'linux' })).toThrow(/unset/u)
  })
})

describe('binary verification', () => {
  const asset: ReleaseAsset = { ...LINUX_ASSET, name: 'zup' }

  it('accepts bytes that match the published identity', () => {
    expect(() => assertVerified(PAYLOAD, asset, silentLog(), 'zup')).not.toThrow()
  })

  it('refuses a size mismatch', () => {
    expect(() => assertVerified(PAYLOAD.slice(0, 3), asset, silentLog(), 'zup')).toThrow(
      /3 bytes and the release says 6/u,
    )
  })

  it('refuses a digest mismatch', () => {
    const tampered = new Uint8Array(PAYLOAD)
    tampered[0] = 0x00
    expect(() => assertVerified(tampered, asset, silentLog(), 'zup')).toThrow(
      /do not match what zup published/u,
    )
  })

  it('refuses when GitHub and zup recorded different digests', () => {
    // Two independent records disagreeing is exactly the event worth stopping for:
    // the asset was replaced after publication.
    expect(() =>
      assertVerified(PAYLOAD, { ...asset, hostDigest: 'b'.repeat(64) }, silentLog(), 'zup'),
    ).toThrow(/Two independent records disagree/u)
  })

  it('accepts when both records agree', () => {
    expect(() =>
      assertVerified(PAYLOAD, { ...asset, hostDigest: PAYLOAD_DIGEST }, silentLog(), 'zup'),
    ).not.toThrow()
  })

  it('treats a digest case-insensitively, because sources differ', () => {
    expect(() =>
      assertVerified(
        PAYLOAD,
        { ...asset, digest: PAYLOAD_DIGEST.toUpperCase() },
        silentLog(),
        'zup',
      ),
    ).not.toThrow()
  })

  it('computes the digest zup records', () => {
    // The empty string's SHA-256, which is a fixed value every implementation agrees
    // on and which proves the algorithm is SHA-256 and not SHA-1.
    expect(digestOf(new Uint8Array())).toBe(
      'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
    )
  })
})

describe('tool resolution', () => {
  it('uses an explicit path without consulting the cache or the network', async () => {
    const { call, downloader } = resolve({ zupPath: '/w/zup', files: { '/w/zup': 'binary' } })
    const tool = await call
    expect(tool.source).toBe('explicit')
    expect(tool.path).toBe('/w/zup')
    expect(downloader.calls).toBe(0)
  })

  it('refuses an explicit path that does not exist', async () => {
    const { call } = resolve({ zupPath: '/w/missing' })
    await expect(call).rejects.toThrow(ToolError)
  })

  it('downloads and verifies on a cache miss', async () => {
    const { call, downloader } = resolve({ zupVersion: '1.4.0' })
    const tool = await call
    expect(tool.source).toBe('download')
    expect(tool.version).toBe('1.4.0')
    expect(downloader.calls).toBe(1)
    expect(tool.path).toBe('/cache/1.4.0-linux-x64/zup-linux-x64')
  })

  it('uses a verified cache hit without downloading', async () => {
    // A warm runner must not touch the network. This is the case that decides
    // whether the action adds meaningful overhead to a build.
    const { call, downloader } = resolve({
      zupVersion: '1.4.0',
      files: { '/cache/1.4.0-linux-x64/zup-linux-x64': PAYLOAD },
      cached: '1.4.0-linux-x64',
    })
    const tool = await call
    expect(tool.source).toBe('cache')
    expect(downloader.calls).toBe(0)
    expect(tool.path).toBe('/cache/1.4.0-linux-x64/zup-linux-x64')
  })

  it('never runs a corrupt cached entry', async () => {
    // A truncated file under the right name: a full disk, a killed job, or a cache
    // directory shared between concurrent jobs. Executing it would be a debugging
    // session with a wrong binary and no error message.
    const { call, downloader } = resolve({
      zupVersion: '1.4.0',
      files: { '/cache/1.4.0-linux-x64/zup-linux-x64': PAYLOAD.slice(0, 3) },
      cached: '1.4.0-linux-x64',
    })
    expect((await call).source).toBe('download')
    expect(downloader.calls).toBe(1)
  })

  it('never runs a cached entry whose bytes were substituted', async () => {
    // Same size, different bytes: a size check alone would accept this.
    const substituted = new Uint8Array(PAYLOAD)
    substituted[0] = 0xff
    const { call, downloader } = resolve({
      zupVersion: '1.4.0',
      files: { '/cache/1.4.0-linux-x64/zup-linux-x64': substituted },
      cached: '1.4.0-linux-x64',
    })
    expect((await call).source).toBe('download')
    expect(downloader.calls).toBe(1)
  })

  it('refuses a version whose asset the release does not have', async () => {
    const { call } = resolve({ zupVersion: '9.9.9', asset: null })
    await expect(call).rejects.toThrow(/has no `zup-linux-x64` asset/u)
  })

  it('refuses a tool whose bytes do not match the published digest', async () => {
    // The whole point of the verification step: an executable is never run on the
    // strength of a URL resolving.
    const tampered = new Uint8Array(PAYLOAD)
    tampered[0] = 0x00
    const { call, filesystem } = resolve({ zupVersion: '1.4.0', payload: tampered })
    await expect(call).rejects.toThrow(/do not match what zup published/u)
    // Nothing was cached: a failed install must not leave a bad entry behind.
    expect([...filesystem.entries()]).toHaveLength(0)
  })

  it('refuses a download the server truncated', async () => {
    // A proxy cut the response: the bytes are a prefix of the real file.
    const { call } = resolve({
      zupVersion: '1.4.0',
      payload: PAYLOAD.slice(0, 2),
      contentLength: PAYLOAD.byteLength,
    })
    await expect(call).rejects.toThrow(/incomplete/u)
  })

  it('refuses a runner zup has no release for, before touching the network', async () => {
    const { call, downloader } = resolve({ zupVersion: '1.4.0', runner: FREEBSD_X64 })
    await expect(call).rejects.toThrow(UnsupportedRunnerError)
    expect(downloader.calls).toBe(0)
  })
})
