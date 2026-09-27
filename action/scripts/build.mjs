// GitHub runs the action straight from the repository, with no install step, so
// `dist/index.js` is committed. The runtime is the Node 24 that `action.yml`
// declares, and nothing in `action/src` may touch a `Bun.*` API.
// `scripts/verify-runtime.mjs` fails if the bundle contains one.

import { build } from 'bun'

// One file, no lazy chunks: a chunk graph is content-hashed, harder to diff, and
// one missing file away from ERR_MODULE_NOT_FOUND on a runner.
const result = await build({
  entrypoints: ['./src/main.ts'],
  // Bun 1.4.2 accepts `outfile` and silently ignores it, writing ./main.js where
  // `action.yml` does not look. Every build step then reports success and the
  // action fails to load. `outdir` plus `naming` is the form that works.
  outdir: './dist',
  naming: { entry: 'index.js' },
  target: 'node',
  format: 'esm',
  // Committed and read by humans, so minified.
  minify: true,
  throw: true,
})

if (!result.success) {
  console.error('bun build did not succeed')
  process.exit(1)
}

for (const log of result.logs) {
  console.log(log)
}

// Must land exactly where `runs.main` in `action.yml` points.
const expected = new URL('../dist/index.js', import.meta.url)
if (!(await Bun.file(expected).exists())) {
  console.error(`expected ${expected.pathname} to exist after a successful build`)
  process.exit(1)
}
console.log(`dist/index.js — ${(await Bun.file(expected).arrayBuffer()).byteLength} bytes`)
