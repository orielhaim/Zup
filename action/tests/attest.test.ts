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

/** A manifest as `zup build --release-manifest` writes it. */
const MANIFEST: ReleaseManifestDocument = {
  schema: 1,
  application: { id: 'com.acme.app', name: 'Acme', version: '1.4.0' },
  root: '.',
  variants: [
    { id: 'x64', target: 'x86_64-pc-windows-msvc', platform: 'windows', frontend: 'gui' },
    { id: 'arm64', target: 'aarch64-pc-windows-msvc', platform: 'windows', frontend: 'gui' },
  ],
  artifacts: [
    {
      id: 'installer-x64',
      kind: 'installer',
      mode: 'standalone',
      path: 'Acme-Windows-x64-Setup.exe',
      digest: 'a'.repeat(64),
      size: 248_512_896,
      signature: { status: 'signed', subject: 'Acme' },
    },
    {
      id: 'universal',
      kind: 'installer',
      mode: 'composed',
      path: 'Acme-Windows-Setup.exe',
      digest: 'b'.repeat(64),
      size: 259_522_560,
    },
    {
      id: 'transport-x64',
      kind: 'package',
      mode: 'transport',
      path: 'Acme-Windows-x64.zup',
      digest: 'c'.repeat(64),
      size: 104_857_600,
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
})

describe('manifestArtifacts', () => {
  it('projects the manifest onto the result shape', () => {
    const artifacts = manifestArtifacts(MANIFEST)
    expect(artifacts.map((entry) => entry.path)).toEqual([
      'Acme-Windows-x64-Setup.exe',
      'Acme-Windows-Setup.exe',
      'Acme-Windows-x64.zup',
    ])
    expect(artifacts[0]?.signature).toBe('signed')
    expect(artifacts[1]?.signature).toBeUndefined()
  })
})

/** The io the subject walk needs, in a form a test controls. */
const io = {
  resolve: (...segments: string[]): string => segments.join('/'),
  exists: async (target: string): Promise<boolean> => target.includes('present'),
  digest: async (): Promise<string> => 'd'.repeat(64),
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

  it('carries the digest the manifest recorded, not a re-derived one', async () => {
    // Attesting a different value than the one that was published would attest
    // nothing, so the manifest is read rather than recomputed.
    const subjects = await attestSubjects(path, MANIFEST, 'dist', [], io)
    const installer = subjects.find((entry) => entry.name.endsWith('Acme-Windows-Setup.exe'))
    expect(installer?.digest).toBe('b'.repeat(64))
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
