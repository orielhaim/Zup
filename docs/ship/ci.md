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

## Repository checks

Pull requests run formatting, Clippy, fast tests, and doctests on Windows and Linux. Each OS uses one job to reuse build artifacts. Documentation-only changes do not run Rust CI. The Action workflow runs when its wrapper, CLI, or dependencies change.

Run **Full Verification** manually for native service tests, staged-toolchain integration, ARM64 terminal tests, and cross-built installers. Its `sdk` and `release` inputs enable generated-project and release checks. **Linux musl** and **Native service VMs** are separate manual workflows. The VM workflow requires dedicated Alpine guests.
