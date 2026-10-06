#!/usr/bin/env node
// `action.yml` and the implementation drift apart silently, so the check runs
// both ways: an input or output the code uses must be declared, and a declared
// one must be used.

import { readdir, readFile, stat } from 'node:fs/promises'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..')
const actionFile = join(root, 'action.yml')

/** Map of the top-level `key: value` pairs inside the named YAML block. */
function topLevelKeys(text, block) {
  const keys = new Map()
  const lines = text.split(/\r?\n/)
  let inside = false
  for (const line of lines) {
    if (new RegExp(`^${block}:`).test(line)) {
      inside = true
      continue
    }
    if (inside) {
      if (/^\S/.test(line)) {
        break
      }
      const entry = /^ {2}([A-Za-z0-9-]+):\s*(.*)$/.exec(line)
      if (entry) {
        keys.set(entry[1], entry[2].trim())
      }
    }
  }
  return keys
}

const problems = []

const metadata = await readFile(actionFile, 'utf8')

const declaredInputs = topLevelKeys(metadata, 'inputs')
const declaredOutputs = topLevelKeys(metadata, 'outputs')

const sources = await readdir(join(root, 'action', 'src'))
const read = async (name) => readFile(join(root, 'action', 'src', name), 'utf8')
const implementation = (
  await Promise.all(sources.filter((name) => name.endsWith('.ts')).map(read))
).join('\n')

// Inputs are read from the frozen table in `inputs.ts`, outputs from `setOutput`.
// The matches are structural, so a refactor of either surfaces as a failed check
// rather than as a silently shorter list.
const inputTable = /const RAW[\s\S]*?Object\.fromEntries\(\s*\[([\s\S]*?)\]\.map/.exec(
  implementation,
)
if (!inputTable) {
  problems.push('could not find the input table in action/src/inputs.ts')
} else {
  for (const [, name] of inputTable[1].matchAll(/'([a-z0-9-]+)'/g)) {
    if (!declaredInputs.has(name)) {
      problems.push(`action.yml does not declare the \`${name}\` input the implementation reads`)
    }
  }
}
for (const name of declaredInputs.keys()) {
  if (!inputTable?.[1].includes(`'${name}'`)) {
    problems.push(`action.yml declares \`${name}\`, which the implementation never reads`)
  }
}
for (const [, name] of implementation.matchAll(/setOutput\(\s*'([a-z0-9-]+)'/g)) {
  if (!declaredOutputs.has(name)) {
    problems.push(`action.yml does not declare the \`${name}\` output the implementation sets`)
  }
}
for (const name of declaredOutputs.keys()) {
  if (!new RegExp(`setOutput\\(\\s*'${name}'`).test(implementation)) {
    problems.push(`action.yml declares the \`${name}\` output, which nothing sets`)
  }
}

const main = /^ {2}main:\s*(\S+)\s*$/m.exec(metadata)
if (!main) {
  problems.push('action.yml has no `runs.main`')
} else {
  const bundle = join(root, main[1])
  const found = await stat(bundle).catch(() => undefined)
  if (!found?.isFile()) {
    problems.push(
      `\`runs.main\` points at ${main[1]}, which does not exist. Run \`npm run build\`.`,
    )
  }
}

if (!/^ {2}using: node24$/m.test(metadata)) {
  problems.push('`runs.using` is not `node24`. The bundle targets Node 24.')
}

// Every key in `runs`, not just `using` and `main`. GitHub rejects the whole
// action when `runs` names a key it does not define, and says so only at the point
// the action is used - so an invented key here fails every job that tries to run
// it, with a message about the metadata rather than about the code. The checks
// above cover the inputs, the outputs and the bundle; a key in `runs` was the one
// thing nothing looked at, which is how `minimum` reached a released action.
const RUNS_KEYS = new Set(['using', 'main', 'pre', 'pre-if', 'post', 'post-if'])
for (const name of topLevelKeys(metadata, 'runs').keys()) {
  if (!RUNS_KEYS.has(name)) {
    problems.push(
      `action.yml declares \`runs.${name}\`, which is not a key GitHub defines for a ` +
        `JavaScript action. Allowed: ${[...RUNS_KEYS].join(', ')}.`,
    )
  }
}

// A released action must default to the zup version it was built and tested
// against, so TESTED_ZUP_VERSION cannot drift from the workspace version.
const cargoToml = await readFile(join(root, 'Cargo.toml'), 'utf8')
const workspaceVersion = /^\[workspace\.package\][\s\S]*?^version = "([^"]+)"/m.exec(cargoToml)
if (!workspaceVersion) {
  problems.push('could not read `[workspace.package] version` from Cargo.toml')
} else {
  const declared = /const TESTED_ZUP_VERSION = '([^']+)'/.exec(implementation)
  if (!declared) {
    problems.push('could not find TESTED_ZUP_VERSION in action/src/workflow.ts')
  } else if (declared[1] !== workspaceVersion[1]) {
    problems.push(
      `the default zup version is ${declared[1]} and the workspace version is ` +
        `${workspaceVersion[1]}. A released action defaults to the version it was ` +
        'built against; update TESTED_ZUP_VERSION or do not release this commit.',
    )
  }
}

if (problems.length > 0) {
  console.error('action.yml does not match the implementation:')
  for (const problem of problems) {
    console.error(`  ${problem}`)
  }
  process.exit(1)
}

console.log(
  `action.yml declares ${declaredInputs.size} inputs and ${declaredOutputs.size} outputs, ` +
    '`runs` points at a bundle that exists, and the default zup version matches the workspace.',
)
