import { describe, expect, it } from 'bun:test'
import type { ReleaseManifestDocument } from '../src/artifacts.js'
import {
  attestSubjects,
  isAlreadyCompressed,
  ManifestError,
  manifestArtifacts,
  parseManifest,
  shouldUploadDirect,
} from '../src/artifacts.js'
import type { SigningEvidence } from '../src/result.js'
import { coversBytes } from '../src/result.js'

/** What a platform adapter hands over once it has verified a signature. */
const SIGNED: SigningEvidence[] = [
  { fact: 'signature_covers_bytes', value: 'sha256' },
  { fact: 'platform_trust_accepted', value: 'windows' },
  { fact: 'publisher', value: 'CN=Acme' },
  { fact: 'certificate', value: '0011AABB' },
  { fact: 'timestamp', value: 'rfc3161' },
]

/** A manifest as `zup build --release-manifest` and `zup sign verify` write it. */
const MANIFEST: ReleaseManifestDocument = {
  schema: 1,
  application: { id: 'com.acme.app', name: 'Acme', version: '1.4.0' },
  root: '.',
  variants: [
    {
      id: 'x64',
      target: 'x86_64-pc-windows-msvc',
      platform: 'windows',
      frontend: 'gui',
      runtime: { digest: '9'.repeat(64), evidence: SIGNED },
    },
    { id: 'arm64', target: 'aarch64-pc-windows-msvc', platform: 'windows', frontend: 'gui' },
  ],
  artifacts: [
    {
      id: 'installer-x64',
      kind: 'single',
      mode: 'offline',
      path: 'Acme-Windows-x64-Setup.exe',
      // Signing appends a certificate table, so the built identity and the
      // published one are different files for every signed artifact.
      built: { digest: 'a'.repeat(64), size: 248_512_896 },
      finalized: { digest: 'b'.repeat(64), size: 249_123_456, evidence: SIGNED },
    },
    {
      id: 'universal',
      kind: 'universal',
      mode: 'offline',
      path: 'Acme-Windows-Setup.exe',
      built: { digest: 'c'.repeat(64), size: 259_522_560 },
      finalized: { digest: 'd'.repeat(64), size: 260_112_640, evidence: SIGNED },
    },
    {
      id: 'transport-x64',
      kind: 'package',
      mode: 'offline',
      path: 'Acme-Windows-x64.zup',
      built: { digest: 'f'.repeat(64), size: 104_857_600 },
      // Finalized with no signature: a real published identity, and nothing in
      // it that says who produced it. `finalized` and `signed` are different
      // questions, and this artifact answers the first and not the second.
      finalized: { digest: '0'.repeat(64), size: 104_857_600, evidence: [] },
    },
  ],
}

const path = 'C:\\w\\dist\\zup-release.json'

describe('parseManifest', () => {
  it('reads a manifest', () => {
    const manifest = parseManifest(JSON.stringify(MANIFEST), path)
    expect(manifest.application.version).toBe('1.4.0')
    expect(manifest.artifacts).toHaveLength(3)
  })

  it('refuses a document that is not JSON', () => {
    expect(() => parseManifest('<html>404</html>', path)).toThrow(ManifestError)
  })

  it('refuses a manifest whose schema it does not know', () => {
    expect(() => parseManifest(JSON.stringify({ ...MANIFEST, schema: 2 }), path)).toThrow(
      /schema 2 and this action reads 1/u,
    )
  })

  it('refuses a manifest with no artifacts', () => {
    // A release with nothing in it is not something to attest or publish, and a
    // silent empty list would turn into a successful-looking release.
    expect(() =>
      parseManifest(JSON.stringify({ ...MANIFEST, artifacts: undefined }), path),
    ).toThrow(/no `artifacts`/u)
  })

  it('refuses a manifest with an artifact that was never finalized', () => {
    // Attesting the built digest would attest a file nobody downloads: signing
    // appends a certificate table, so those bytes are not the ones released.
    const unfinalized = MANIFEST.artifacts.map((artifact, index) =>
      index === 2 ? { ...artifact, finalized: undefined } : artifact,
    )
    expect(() =>
      parseManifest(JSON.stringify({ ...MANIFEST, artifacts: unfinalized }), path),
    ).toThrow(/no `finalized` identity for .*Acme-Windows-x64\.zup/u)
  })
})

describe('manifestArtifacts', () => {
  it('projects the manifest onto the result shape', () => {
    const artifacts = manifestArtifacts(MANIFEST)
    expect(artifacts.map((entry) => entry.path)).toEqual([
      'Acme-Windows-x64-Setup.exe',
      'Acme-Windows-Setup.exe',
      'Acme-Windows-x64.zup',
    ])
    // The published identity, not the built one.
    expect(artifacts[0]?.digest).toBe('b'.repeat(64))
    expect(artifacts[0]?.size).toBe(249_123_456)
    expect(coversBytes(artifacts[0]?.evidence)).toBe(true)
    // A finalized release with no signature is not a signed one, and the
    // envelope says so rather than leaving the field out.
    expect(artifacts[2]?.digest).toBe('0'.repeat(64))
    expect(artifacts[2]?.evidence).toEqual([])
    expect(coversBytes(artifacts[2]?.evidence)).toBe(false)
  })
})

/**
 * The io the subject walk needs, in a form a test controls.
 *
 * `ON_DISK` is what the files actually hash to, and it is mutable so a test can
 * substitute bytes for bytes that were published.
 */
const ON_DISK: Record<string, string> = {
  'Acme-Windows-x64-Setup.exe': 'b'.repeat(64),
  'Acme-Windows-Setup.exe': 'd'.repeat(64),
  'Acme-Windows-x64.zup': '0'.repeat(64),
}

const io = {
  resolve: (...segments: string[]): string => segments.join('/'),
  exists: async (target: string): Promise<boolean> => target.includes('present'),
  digest: async (target: string): Promise<string> =>
    ON_DISK[target.slice(target.lastIndexOf('/') + 1)] ?? 'e'.repeat(64),
}

describe('attestSubjects', () => {
  it('always includes the manifest and every artifact it names', async () => {
    const subjects = await attestSubjects(path, MANIFEST, 'dist', [], io)
    expect(subjects.map((entry) => entry.name)).toEqual([
      path,
      'dist/Acme-Windows-Setup.exe',
      'dist/Acme-Windows-x64-Setup.exe',
      'dist/Acme-Windows-x64.zup',
    ])
  })

  it('carries the published digest, not the built one', async () => {
    // The built digest names pre-signature bytes, and attesting those would
    // attach provenance to a file that no downloader ever receives.
    const subjects = await attestSubjects(path, MANIFEST, 'dist', [], io)
    const installer = subjects.find((entry) => entry.name.endsWith('Acme-Windows-Setup.exe'))
    expect(installer?.digest).toBe('d'.repeat(64))
  })

  it('refuses a file that changed after the release was finalized', async () => {
    // Re-derived rather than read. Attaching provenance to bytes the release
    // does not claim is the failure attestation exists to prevent, so this is a
    // refusal and not a warning.
    const published = ON_DISK['Acme-Windows-Setup.exe'] as string
    ON_DISK['Acme-Windows-Setup.exe'] = '7'.repeat(64)
    try {
      await expect(attestSubjects(path, MANIFEST, 'dist', [], io)).rejects.toThrow(
        /changed after `zup sign verify` ran/u,
      )
    } finally {
      ON_DISK['Acme-Windows-Setup.exe'] = published
    }
  })

  it('is sorted, so a summary and a test see the same order', async () => {
    const subjects = await attestSubjects(path, MANIFEST, 'dist', [], io)
    expect([...subjects.map((entry) => entry.name)].sort()).toEqual(
      subjects.map((entry) => entry.name),
    )
  })

  it('does not glob the release directory', async () => {
    // The whole point: an intermediate per-target artifact that compose merged
    // away must not be attested, because nobody will ever download it.
    const subjects = await attestSubjects(path, MANIFEST, 'dist', [], io)
    expect(subjects.map((entry) => entry.name)).not.toContain('dist/variants/x64/Acme.exe')
  })

  it('includes an extra path the project asked for', async () => {
    const subjects = await attestSubjects(path, MANIFEST, 'dist', ['checksums/present.txt'], io)
    expect(subjects.map((entry) => entry.name)).toContain('dist/checksums/present.txt')
  })

  it('refuses an extra path that does not exist', async () => {
    // An attestation of a file nobody receives is a claim nothing can check, and
    // a silently smaller subject set is worse than a failure.
    await expect(attestSubjects(path, MANIFEST, 'dist', ['missing.txt'], io)).rejects.toThrow(
      ManifestError,
    )
    await expect(attestSubjects(path, MANIFEST, 'dist', ['missing.txt'], io)).rejects.toThrow(
      /does not exist/u,
    )
  })

  it('de-duplicates a subject named twice on its own', async () => {
    const subjects = await attestSubjects(
      path,
      MANIFEST,
      'dist',
      ['notes/present.txt', 'notes/present.txt'],
      io,
    )
    expect(subjects.filter((entry) => entry.name.endsWith('present.txt'))).toHaveLength(1)
  })
})

describe('compression', () => {
  it('recognises the formats zup produces that do not compress', () => {
    for (const path of [
      'Acme-Windows-Setup.exe',
      'Acme.msi',
      'Acme-Windows-x64.zup',
      'Acme.tar.zst',
      'Acme.tgz',
      'Acme.zip',
    ]) {
      expect(isAlreadyCompressed(path)).toBe(true)
    }
  })

  it('treats an unknown extension as needing a zip', () => {
    // A wrong guess here is a broken download, so the default is the safe one.
    expect(isAlreadyCompressed('Acme-Windows-Setup')).toBe(false)
    expect(isAlreadyCompressed('notes.txt')).toBe(false)
  })

  it('matches an extension case-insensitively', () => {
    expect(isAlreadyCompressed('ACME.EXE')).toBe(true)
  })

  it('uploads a single compressed file directly', () => {
    expect(shouldUploadDirect(['/w/dist/Acme.exe'])).toBe(true)
  })

  it('never uploads a directory directly', () => {
    // The service rejects `skipArchive` for more than one path, and a directory
    // is not one path.
    expect(shouldUploadDirect(['/w/dist'])).toBe(false)
    expect(shouldUploadDirect(['/w/a.exe', '/w/b.exe'])).toBe(false)
    expect(shouldUploadDirect(['/w/dist/notes.txt'])).toBe(false)
  })
})
