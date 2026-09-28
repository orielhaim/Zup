/**
 * Reading the release manifest to decide what is worth attesting.
 *
 * The final bytes of a release are the ones on disk after signing, and
 * `zup-release.json` names all of them with a digest and a size measured from those
 * bytes. It is written by the same code that published them and is the document a
 * consumer verifies against, so it is the authoritative list.
 *
 * A glob over the build directory is wrong twice: it picks up the per-target
 * intermediates that compose merged away, and it picks up whatever else is in the
 * directory - including a signing key somebody left there, which would then be
 * published as an attested subject.
 *
 * The manifest is always attested too. It names every other digest, so an
 * attestation of anything without an attestation of it is a chain with a missing link.
 *
 * **A manifest that is not finalized is refused, not read.** An artifact carries
 * two identities: what composition produced, and what will be published. Signing
 * changes the bytes, so for a signed artifact they differ, and attesting the first
 * attests a file nobody downloads. A missing `finalized` block means the release
 * has not been signed and verified, and the only honest response is to stop.
 */

import { createHash } from 'node:crypto'
import { readFile } from 'node:fs/promises'
import type { AttestationSubject } from './ports.js'
import type { SigningEvidence } from './protocol.js'

/** The manifest's own shape, as far as this needs it. */
export interface ReleaseManifestDocument {
  schema: number
  application: { id: string; name: string; version: string }
  root: string
  variants: {
    id: string
    target: string
    platform: string
    frontend: string
    runtime?: { digest: string; evidence?: SigningEvidence[] }
  }[]
  artifacts: {
    id: string
    kind: string
    mode: string
    path: string
    /** What composition produced, before any signature. */
    built: { digest: string; size: number }
    /**
     * What will be published, and what the platform established about it.
     *
     * `evidence` is empty on an unsigned release, which is a real release: it is
     * finalized, it has a published identity, and nothing proves who produced it.
     * So "finalized" and "signed" are separate questions about the same block.
     */
    finalized?: { digest: string; size: number; evidence: SigningEvidence[] } | null
  }[]
}

/** Why a manifest could not be used. */
export class ManifestError extends Error {
  constructor(
    message: string,
    readonly remedy: string,
  ) {
    super(`${message} ${remedy}`)
    this.name = 'ManifestError'
  }
}

/** The supported manifest schema. */
export const MANIFEST_SCHEMA = 1

/**
 * Parse and check a release manifest.
 *
 * Every check is a refusal rather than a repair: skipping any of them produces an
 * attestation of the wrong bytes.
 */
export function parseManifest(text: string, path: string): ReleaseManifestDocument {
  let parsed: unknown
  try {
    parsed = JSON.parse(text)
  } catch (error) {
    throw new ManifestError(
      `\`${path}\` is not JSON: ${(error as Error).message}.`,
      'It is written by `zup build --release-manifest`; run a build first.',
    )
  }
  if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) {
    throw new ManifestError(
      `\`${path}\` is not a release manifest.`,
      'It is written by `zup build --release-manifest`; run a build first.',
    )
  }
  const value = parsed as Record<string, unknown>
  const schema = value['schema']
  if (schema !== MANIFEST_SCHEMA) {
    throw new ManifestError(
      `\`${path}\` has schema ${String(schema)} and this action reads ${MANIFEST_SCHEMA}.`,
      'Upgrade the action, or pin `zup-version` to a release whose manifest it reads.',
    )
  }
  if (typeof value['artifacts'] !== 'object' || !Array.isArray(value['artifacts'])) {
    throw new ManifestError(
      `\`${path}\` has no \`artifacts\`.`,
      'A release manifest without artifacts describes nothing to attest.',
    )
  }
  const manifest = value as unknown as ReleaseManifestDocument
  const withoutFinal = manifest.artifacts.filter((artifact) => artifact.finalized === undefined)
  if (withoutFinal.length > 0) {
    throw new ManifestError(
      `\`${path}\` has no \`finalized\` identity for ${withoutFinal
        .map((artifact) => `\`${artifact.path}\``)
        .join(', ')}.`,
      'It was written by a zup that does not finalize releases, or by hand. This ' +
        'action will not attest pre-signature digests, because they name bytes no ' +
        'downloader receives.',
    )
  }
  return manifest
}

/**
 * The files to attest, as absolute paths.
 *
 * A path that does not exist is a refusal: an attestation step that quietly attests
 * fewer files than the project asked for produces a release that looks attested and
 * is not.
 */
export async function attestSubjects(
  manifestPath: string,
  manifest: ReleaseManifestDocument,
  releaseDir: string,
  extra: string[],
  io: {
    resolve: (...segments: string[]) => string
    exists: (path: string) => Promise<boolean>
    digest: (path: string) => Promise<string>
  },
): Promise<AttestationSubject[]> {
  const subjects = new Map<string, AttestationSubject>()
  // The manifest is not one of its own artifacts, so its digest is measured. An
  // artifact's digest is *read* rather than re-derived: attesting a value other
  // than the one that was published would attest nothing.
  subjects.set(manifestPath, { name: manifestPath, digest: await io.digest(manifestPath) })
  for (const artifact of manifest.artifacts) {
    const path = io.resolve(releaseDir, artifact.path)
    if (subjects.has(path)) {
      continue
    }
    const final = artifact.finalized
    if (final === undefined || final === null) {
      throw new ManifestError(
        `\`${artifact.path}\` has no final identity.`,
        'Sign the release and run `zup sign verify` before attesting it. An ' +
          'attestation of pre-signature bytes is a claim about a file that no ' +
          'downloader will ever receive.',
      )
    }
    // Re-derived rather than read. A finalized manifest names what was
    // published, and attesting a value that disagrees with the bytes on disk
    // attaches provenance to something else entirely - which is the failure
    // attestation exists to prevent, so the check is a refusal, not a warning.
    const measured = await io.digest(path)
    if (measured !== final.digest) {
      throw new ManifestError(
        `\`${artifact.path}\` is sha256:${measured} and the finalized release says ` +
          `sha256:${final.digest}.`,
        'The file changed after `zup sign verify` ran. Re-verify the release; ' +
          'attesting these bytes would attach provenance the release does not claim.',
      )
    }
    subjects.set(path, { name: path, digest: measured })
  }
  for (const entry of extra) {
    const candidate = io.resolve(releaseDir, entry)
    if (!(await io.exists(candidate))) {
      throw new ManifestError(
        `\`attest-paths\` names ${entry}, which does not exist under ${releaseDir}.`,
        'Remove it, or make sure the build produced it. An attestation of a file ' +
          'that is not there would be a claim nothing can check.',
      )
    }
    if (!subjects.has(candidate)) {
      subjects.set(candidate, { name: candidate, digest: await io.digest(candidate) })
    }
  }
  return [...subjects.values()].sort((left, right) => left.name.localeCompare(right.name))
}

/**
 * Whether a payload is already compressed.
 *
 * The rule is deliberately narrow - a directory always needs the zip, and an
 * unknown extension gets the zip, because a wrong guess here is a broken download.
 */
export function isAlreadyCompressed(path: string): boolean {
  const lower = path.toLowerCase()
  return [
    '.exe',
    '.msi',
    '.zup',
    '.tar.zst',
    '.tar.gz',
    '.tgz',
    '.zst',
    '.gz',
    '.zip',
    '.7z',
    '.xz',
  ].some((extension) => lower.endsWith(extension))
}

/** Whether an upload should use the direct, unarchived artifact path. */
export function shouldUploadDirect(paths: string[]): boolean {
  return paths.length === 1 && isAlreadyCompressed(paths[0] as string)
}

/** The SHA-256 of a file's bytes, for an attestation subject. */
export async function digestOfFile(path: string): Promise<string> {
  return createHash('sha256')
    .update(await readFile(path))
    .digest('hex')
}
