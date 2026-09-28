# action/

The implementation of the zup GitHub Action. `action.yml` at the repository root
is the metadata the runner reads; this directory is the code behind it.

```text
Bun ── installs, tests, bundles ──▶ dist/index.js ── runs on ──▶ Node 24
```

Two runtimes, and the split is the point. Bun is the toolchain; Node 24 is what
GitHub's runner actually executes. Three gates keep that honest:

- `tsconfig.json` covers `action/src` with `"types": ["node"]` and nothing else,
  so a `Bun.*` API in the shipped code is a **compile error**. `tsconfig.test.json`
  gives the tests Bun's globals, because they run under Bun.
- `bun run build` passes `--target=node`, and `scripts/verify-runtime.mjs` fails
  on any `Bun.*` that arrives from a dependency.
- `scripts/verify-runtime.mjs` runs the built bundle as a child process under Node
  and inspects what it wrote. A bundle Bun likes and Node cannot load is a green CI
  run and a broken release in somebody else's workflow.

```text
src/
  main.ts        the entry point, and nothing else
  workflow.ts    read inputs, install zup, run the phases, report
  inputs.ts      the input table, its parsing, and argument tokenization
  phases.ts      which zup command each workflow phase runs
  protocol.ts    decoding zup's automation protocol
  stream.ts      chunk-safe line framing for the protocol stream
  tool.ts        installing the CLI, and verifying the bytes
  platform.ts    runner identity, and the asset name it maps to
  artifacts.ts   reading the release description, and deciding what to attest
  security.ts    refusing a mutation on a dangerous trigger
  summary.ts     the job summary
  ports.ts       every effect the action has, as an interface
  runtime.ts     the real implementations of those interfaces
scripts/
  build.mjs             bun build → dist/index.js
  check-metadata.mjs    action.yml against the implementation
  verify-runtime.mjs    the bundle, under Node 24
dist/            the committed bundle: one file
tests/           unit tests, run by bun test
```

`src/protocol.generated.ts` is generated from the same Rust DTOs as
`schema/automation-v1.schema.json`, by `cargo xtask automation generate`. The
decoder beside it is hand-written, because that is where the
ignore-what-you-do-not-know rule lives. See
[`../docs/automation.md`](../docs/automation.md).

The tests read the golden documents from `fixtures/automation/` on disk - the ones
zup's own Rust serializes - so `bun test` is a compatibility gate rather than only
coverage.

The split between `ports.ts` and everything else is the one that matters for the
tests. Every effect goes through an interface, and `runtime.ts` is the only file
that touches `process.env`, `child_process`, `node:fs` or the network. That is
what lets the tests assert "the build subprocess does not receive the token" as a
fact about the code rather than as a fact about a log line.

```bash
bun install --frozen-lockfile
bun run check          # everything, including the Node 24 run
bun run check:dist     # the committed bundle is current
bun run local          # run the action on this machine
```

Read [`../docs/action.md`](../docs/action.md) first. It covers the inputs, the
outputs, the security model and the versioning.
