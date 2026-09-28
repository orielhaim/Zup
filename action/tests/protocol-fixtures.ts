import { readdirSync, readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

/**
 * The golden fixtures, produced by `cargo xtask automation generate`.
 *
 * Read from disk rather than transcribed into this file, because a hand-written
 * copy of a document the *other side* produces is the exact thing this protocol
 * exists to remove. If zup changes a field, these tests fail until the fixture
 * changes with it — which is the compatibility gate, and it is a real one.
 *
 * Every test in this suite that claims "the action reads what zup writes" is
 * standing on these files.
 */
const FIXTURES = join(dirname(fileURLToPath(import.meta.url)), '..', '..', 'fixtures', 'automation')

/** One fixture, by name without its extension. */
export function fixture(name: string): string {
  return readFileSync(join(FIXTURES, `${name}.json`), 'utf8')
}

/** One fixture as a parsed document, for a test that mutates it first. */
export function document(name: string): Record<string, unknown> {
  return JSON.parse(fixture(name)) as Record<string, unknown>
}

/** The stream fixture, split into its lines. */
export function streamLines(): string[] {
  return readFileSync(join(FIXTURES, 'jsonl-progress.jsonl'), 'utf8')
    .split('\n')
    .filter((line) => line.trim().length > 0)
}

/** Every fixture name, so a test can prove the set did not shrink unnoticed. */
export function fixtureNames(): string[] {
  return readdirSync(FIXTURES)
    .map((file) => file.replace(/\.(json|jsonl)$/u, ''))
    .sort()
}
