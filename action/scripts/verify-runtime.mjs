#!/usr/bin/env node
// Bun builds the action; GitHub's Node 24 runtime runs it. `bun build` succeeds
// on bundles Node cannot load, so this is the only place that split shows up.
// Run it with `node`: under `bun` the `typeof Bun` check below fails instead of
// testing the wrong runtime.

import { spawn } from 'node:child_process'
import { mkdtemp, readdir, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const bundle = join(root, 'dist', 'index.js')
const problems = []
const check = (condition, problem) => {
  if (!condition) {
    problems.push(problem)
  }
}

let source = ''
try {
  source = await import('node:fs').then((fs) => fs.readFileSync(bundle, 'utf8'))
} catch (error) {
  console.error(`cannot read ${bundle}: ${error.message}`)
  console.error('Run `bun run build` first.')
  process.exit(1)
}

const distEntries = await readdir(join(root, 'dist'))
check(
  distEntries.length === 1 && distEntries[0] === 'index.js',
  `action/dist holds ${distEntries.join(', ')}; it must hold exactly index.js. ` +
    'A lazily-loaded chunk graph is a bundle that is one missing file away from ' +
    'ERR_MODULE_NOT_FOUND on a runner.',
)

// The GitHub Toolkit is ESM-only, so a CommonJS bundle could not require it.
check(
  !/^\s*(const|var|let)\s+[\w{},\s]+\s*=\s*require\(/m.test(source),
  'the bundle looks like CommonJS; action.yml declares a node24 ESM action',
)
check(
  /^\s*(import|export)\s/m.test(source) || /from\s*["']/.test(source),
  'the bundle has no ESM syntax at all, which is not what a node24 action expects',
)

check(
  typeof Bun === 'undefined',
  'this check must run under Node. `bun scripts/verify-runtime.mjs` tests the wrong runtime.',
)
const major = Number(process.versions.node.split('.')[0])
check(
  major >= 24,
  `this check runs on Node ${process.versions.node}; action.yml declares node24. ` +
    'Run it on Node 24 or newer, which is what the runner provides.',
)

const bunApis = [...source.matchAll(/\bBun\.(?!versions)/g)]
check(
  bunApis.length === 0,
  `the bundle calls Bun.${bunApis[0]?.[0]?.replace('Bun.', '') ?? '<api>'} ` +
    `${bunApis.length} time(s). It runs on Node 24, not on Bun. ` +
    '--target=node is what prevents this; check the build script still passes it.',
)

// `@actions/tool-cache` assigns `__dirname` for its `extract7z` helper, which
// zup never calls, and Bun inlines this machine's directory into it. So the path
// is allowed only as a single dead `var __dirname="…"` assignment. A second
// reference means something reads it, and that is an ENOENT on a runner.
const machinePaths = [
  ...source.matchAll(
    /(?:"|')([A-Za-z]:\\[^"'\n]{6,}|[/\\]{2}[A-Za-z0-9_.-]+[/\\][^"'\n]{6,})(?:"|')/g,
  ),
].map((match) => match[1])

const unique = [...new Set(machinePaths)]
for (const path of unique) {
  const references = source.split(path).length - 1
  // Positional, not a pattern: the minifier emits `var __dirname="…"` with no
  // space and escaped backslashes, so a pattern quietly stops matching.
  const position = source.indexOf(path)
  const prefix = source.slice(Math.max(0, position - 20), position)
  const declaredAsDirname = prefix.endsWith('var __dirname="')
  check(
    references === 1 && declaredAsDirname,
    `the bundle bakes in the build-machine path "${path}" with ${references} ` +
      `reference(s)${declaredAsDirname ? '' : ', and it is not a dead __dirname assignment'}. ` +
      'A path from this machine does not exist on a runner.',
  )
}

const workspace = await mkdtemp(join(tmpdir(), 'zup-bundle-'))
try {
  // The action's first question is `--version`; on Windows it appends `.exe`.
  const stub = join(workspace, process.platform === 'win32' ? 'zup.exe' : 'zup')
  await writeFile(
    stub,
    '#!/bin/sh\n' +
      'for arg in "$@"; do\n' +
      '  if [ "$arg" = "--version" ]; then echo "zup 0.0.1-stub"; exit 0; fi\n' +
      'done\n' +
      'echo "stub zup: not implemented" >&2\n' +
      'exit 64\n',
    { mode: 0o755 },
  )

  const outputs = join(workspace, 'outputs')
  await writeFile(outputs, '')
  // `@actions/core`'s summary writer appends rather than creates, so the runner
  // has to have made this file first.
  const summary = join(workspace, 'summary.md')
  await writeFile(summary, '')

  // `@actions/core` turns inputs into `INPUT_` env vars: uppercased, spaces to
  // underscores, hyphens left alone. So `zup-path` is `INPUT_ZUP-PATH`.
  const input = (name) => `INPUT_${name.replaceAll(' ', '_').toUpperCase()}`

  const ran = await new Promise((resolveRun) => {
    const child = spawn(process.execPath, [bundle], {
      env: {
        ...process.env,
        [input('operation')]: 'setup',
        [input('zup-path')]: stub,
        RUNNER_OS: 'linux',
        RUNNER_ARCH: 'X64',
        GITHUB_OUTPUT: outputs,
        GITHUB_STEP_SUMMARY: summary,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let stdout = ''
    let stderr = ''
    child.stdout.on('data', (chunk) => (stdout += chunk))
    child.stderr.on('data', (chunk) => (stderr += chunk))
    child.on('close', (code) => resolveRun({ code, stdout, stderr }))
  })

  check(
    ran.code === 0,
    `the bundle exited ${ran.code} on Node ${process.versions.node}. ` +
      `stdout: ${ran.stdout.slice(-500)} stderr: ${ran.stderr.slice(0, 500)}`,
  )
  check(
    ran.stdout.includes('::group::Setup zup'),
    `the bundle did not open its first log group. stdout: ${ran.stdout.slice(0, 300)}`,
  )
  // `RUNNER_TOOL_CACHE` is deliberately unset, so reaching for the tool cache at
  // all means `zup-path` was dropped.
  check(
    ran.stdout.includes(`Using the zup at ${stub}`),
    'the bundle did not use the `zup-path` it was given',
  )
  check(
    !ran.stdout.includes('RUNNER_TOOL_CACHE'),
    'the bundle consulted the tool cache despite an explicit `zup-path`',
  )
  check(
    !ran.stdout.includes('has no release for'),
    'the bundle tried to download a tool despite an explicit `zup-path`',
  )
  check(
    !ran.stderr.includes('Cannot find module') && !ran.stderr.includes('ERR_MODULE'),
    `the bundle could not be loaded by Node. stderr: ${ran.stderr.slice(0, 500)}`,
  )
  const written = await readFile(summary, 'utf8').catch(() => '')
  check(
    written.includes('## zup'),
    'the bundle did not write a job summary. That is what `core.summary` does on ' +
      'Node: append to the file GITHUB_STEP_SUMMARY names.',
  )
} finally {
  await rm(workspace, { recursive: true, force: true })
}

if (problems.length > 0) {
  console.error('dist/index.js is not a Node 24 artifact:')
  for (const problem of problems) {
    console.error(`  ${problem}`)
  }
  process.exit(1)
}

console.log(
  `dist/index.js runs on Node ${process.versions.node}: one ESM file, no Bun APIs, ` +
    'no build-machine paths, and it resolved its inputs.',
)
