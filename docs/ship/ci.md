# Release CI

Zup can generate the GitHub Actions release workflow from project configuration.

```bash
zup ci github generate
```

The default output is:

```text
.github/workflows/release.yml
```

Commit that file. It is part of the release definition and should be reviewed.

## Check drift

```bash
zup ci github check
```

Use this in CI to fail when the committed workflow no longer matches the manifest/generator.

## GitHub Action

The repository also exposes `orielhaim/zup@action-v1`. A compact release step is:

```yaml
permissions:
  contents: write

steps:
  - uses: actions/checkout@v7
  - uses: orielhaim/zup@action-v1
    with:
      operation: release
```

The action supports separate `setup`, `build`, `compose`, `finalize`, `attest`, `publish`, and `release` operations when a pipeline needs explicit phases.

Prefer the generated workflow when you want Zup to keep the target matrix and release steps synchronized with `zup.toml`.

## Repository verification

Pull requests run fast tests and doctests on Windows x64 and Linux x64,
plus terminal lifecycle tests on both ARM64 hosts. Full Verification runs
only the staged-toolchain integration cases on the two x64 hosts. It reuses
their tooling to compose a Linux installer on Windows and a Universal Windows
installer on Linux, then runs each installer on its target host. Nextest stops
individual tests after five minutes and publishes failure diagnostics.

Generated SDK project checks and release clean-room verification are opt-in
through the Full Verification workflow's `sdk` and `release` inputs.
Size measurements run locally. The Action workflow uses one cached debug
Windows x64 CLI for its real-tool smoke test.
