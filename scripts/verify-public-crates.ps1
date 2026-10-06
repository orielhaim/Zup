# Verify the published authoring surface the way a consumer outside this
# repository would see it.
#
# There is exactly one crate an author depends on:
#
#   zup-sdk    the facade, with a feature per authoring role
#
# and two roles behind it, which share a name and almost nothing else:
#
#   preset    a window, written in Rust against GPUI
#   plugin    a declaration, compiled to a WebAssembly component
#
# A preset project and a plugin project resolve differently, reach different
# dependency graphs, and must not be able to reach each other's machinery. This
# script proves that, from a directory outside this workspace, against the
# packaged archives of the crates beneath the facade rather than the workspace's
# copies - because a crate that only resolves because of a path this repository
# happens to provide has not been shown to be publishable.
#
# So this proves five things, in the order they stop being true:
#
#   1. Every published crate packages and verifies on its own.
#   2. No published manifest names a crate that exists only here.
#   3. `zup-sdk --features preset` resolves, builds and tests from outside, and
#      its graph reaches GPUI and no WebAssembly runtime.
#   4. `zup-sdk --features plugin` resolves, builds and tests from outside, and
#      its graph reaches no GPUI at all.
#   5. Neither role reaches an internal crate.
#
# The one thing this cannot prove is what `cargo publish` does for the crates
# above `zup-plugin-abi` once it is on crates.io. That is a property of the
# registry rather than of this repository, and it is checked by publishing.
#
# Run: ./scripts/verify-public-crates.ps1

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    # Published for use outside this repository. The facade is the only one an
    # author names; the rest are published because Cargo resolves a transitive
    # dependency from crates.io, not because anyone should reach them directly.
    $published = @(
        "zup-sdk",
        "zup-preset-sdk",
        "zup-preset-sdk-macros",
        "zup-preset-protocol",
        "zup-preset-ipc",
        "zup-plugin-sdk",
        "zup-plugin-abi"
    )

    # Everything else. A published crate that names one of these is a crate that
    # can only be built inside the repository that owns it.
    $internal = @(
        "zup", "zup-core", "zup-runtime", "zup-plan", "zup-exec", "zup-windows",
        "zup-bundle", "zup-installer", "zup-artifact", "zup-transaction",
        "zup-presentation", "zup-build", "zup-manifest", "zup-update",
        "zup-bootstrap", "zup-platform", "zup-acquire", "zup-acquire-http",
        "zup-pe", "zup-binary", "zup-signing", "zup-toolchain", "zup-protocol",
        "zup-dispatch", "zup-preview", "zup-publish", "zup-preset-host",
        "zup-preset-compose", "zup-preset-dev", "zup-preset-default",
        "zup-preset-test", "zup-plugin-contract", "zup-plugin-runtime",
        "zup-plugin-build", "zup-automation", "zup-assets", "zup-xtask"
    )

    # 1. Each published crate packages on its own.
    #
    # Every one of these is new, so none of them is on crates.io yet, and Cargo
    # resolves a dependency by version rather than by path when it packages. Left
    # alone that means packaging the facade fails with "no matching package named
    # `zup-preset-sdk` found" - a fact about what has been published, not about
    # whether the crate can be packaged at all. So every sibling this repository
    # also owns is patched to its local path, which is what the registry supplies
    # once the crate beneath has been published.
    #
    # Cargo's `--config` names a file, so the patch is written rather than
    # inlined: the whole document is one argument, and a shell that split it would
    # hand Cargo fragments.
    $patchFile = Join-Path $root "target\public-crate-check-patch.toml"
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $patchFile) | Out-Null
    $patched = "[patch.crates-io]`n"
    foreach ($other in $published) {
        $patched += "$other = { path = `"crates/$other`" }`n"
    }
    Set-Content -LiteralPath $patchFile -Value $patched -NoNewline
    foreach ($crate in $published) {
        cargo package -p $crate --allow-dirty --no-verify --config $patchFile
        if ($LASTEXITCODE -ne 0) { throw "$crate does not package on its own" }
    }

    # 2. No published manifest reaches a crate that exists only here. Checked on
    #    the source manifests so it covers `cargo package`'s rewriting as well as
    #    the declared shape.
    foreach ($crate in $published) {
        $declared = Get-Content -LiteralPath "crates\$crate\Cargo.toml" -Raw
        # A crate that could not be packaged has already failed above, so what
        # matters here is that none of them opts out of being published.
        if ($declared -match "(?m)^\s*publish\s*=\s*false") { throw "$crate is not published" }
        foreach ($name in $internal) {
            if ($declared -match "(?m)^\s*$name\s*=") { throw "$crate depends on $name" }
            if ($declared -match "(?m)^\s*path\s*=.*$name") {
                throw "$crate has a path dependency on $name"
            }
        }
    }

    # 3 and 4. Each role, resolved and built from outside this workspace.
    #
    # A copy of the facade is placed beside copies of everything it needs, with
    # every path rewritten to a sibling. Nothing it builds can then come from this
    # repository, so a crate that only resolves here is caught here.
    $work = Join-Path $root "target\public-crate-check"
    if (Test-Path $work) { Remove-Item -Recurse -Force $work }
    New-Item -ItemType Directory -Path $work | Out-Null

    # `foo.workspace = true` is inheritance from the root manifest, so a copy that
    # declares its own `[workspace]` - which is what detaching it requires - has
    # nothing left to inherit from. Every inherited name is replaced with the
    # entry the root declares, which is also the only honest way to check these
    # crates outside this repository: what a consumer resolves is the manifest
    # after inheritance, not the manifest as written.
    $rootManifest = Get-Content -LiteralPath (Join-Path $root "Cargo.toml") -Raw
    # The root manifest writes a dependency either as a bare version - `serde = "1"`,
    # which means `{ version = "1" }` - or as an inline table. Both are recorded as
    # the body of a table, because that is the only shape both spellings above
    # substitute into.
    $inherited = @{}
    $table = ($rootManifest -split "(?m)^\[workspace\.dependencies\]")[1] -split "(?m)^\[" | Select-Object -First 1
    foreach ($line in ($table -split "`n")) {
        if ($line -match "^\s*([A-Za-z0-9_-]+)\s*=\s*(\{.*\}|\S+)\s*$") {
            $value = $Matches[2].Trim()
            if ($value.StartsWith("{")) {
                $inherited[$Matches[1]] = $value.TrimStart("{").TrimEnd("}").Trim()
            } else {
                $inherited[$Matches[1]] = "version = $value"
            }
        }
    }

    # `[workspace.package]` is inherited the same way and has to be resolved too:
    # `edition.workspace = true` is as dead a reference in a detached copy as
    # `serde.workspace = true`.
    $packageFields = @{}
    $packageTable = ($rootManifest -split "(?m)^\[workspace\.package\]")[1] -split "(?m)^\[" | Select-Object -First 1
    foreach ($line in ($packageTable -split "`n")) {
        if ($line -match "^\s*([A-Za-z0-9_-]+)\s*=\s*(.+?)\s*$") {
            $packageFields[$Matches[1]] = $Matches[2]
        }
    }

    foreach ($crate in $published) {
        $outside = Join-Path $work $crate
        New-Item -ItemType Directory -Path $outside | Out-Null
        Copy-Item -Recurse -Force "crates\$crate\*" $outside
        # An empty `[workspace]` detaches the copy: this is the whole point, that
        # nothing it builds is inherited from the repository it came from.
        Add-Content -LiteralPath (Join-Path $outside "Cargo.toml") -Value "`n[workspace]"

        $manifest = Join-Path $outside "Cargo.toml"
        $text = Get-Content -LiteralPath $manifest -Raw
        foreach ($other in $published) {
            if ($other -eq $crate) { continue }
            $text = $text -replace "$other = \{ path = `"\.\./$other`", version = `"[0-9.]+`" \}", "$other = { path = `"../$other`" }"
        }
        foreach ($name in $inherited.Keys) {
            $text = $text -replace "(?m)^(\s*)$name\.workspace = true\s*$", "`$1$name = { $($inherited[$name]) }"
            $text = $text -replace "(?m)^(\s*)$name = \{ workspace = true \}\s*$", "`$1$name = { $($inherited[$name]) }"
        }
        foreach ($name in $packageFields.Keys) {
            $text = $text -replace "(?m)^(\s*)$name\.workspace = true\s*$", "`$1$name = $($packageFields[$name])"
        }
        Set-Content -LiteralPath $manifest -Value $text -NoNewline
    }

    # The facade, with the preset role.
    Push-Location (Join-Path $work "zup-sdk")
    try {
        cargo test --all-targets --no-default-features --features preset
        if ($LASTEXITCODE -ne 0) { throw "the preset role does not build outside this repository" }
        $graph = cargo tree --edges normal --prefix none --no-default-features --features preset
        if ($LASTEXITCODE -ne 0) { throw "could not read the preset graph" }
        foreach ($name in $internal) {
            if ($graph | Select-String -Pattern "(^|[^a-z-])$name v" -Quiet) {
                throw "the preset role reaches the internal crate $name"
            }
        }
        foreach ($expected in @("zup-preset-sdk", "zup-preset-protocol", "zup-preset-ipc")) {
            if (-not ($graph | Select-String -Pattern "(^|[^a-z-])$expected v" -Quiet)) {
                throw "the preset role does not link $expected"
            }
        }
        if (-not ($graph | Select-String -Pattern "(^|[^a-z-])gpui-kit v" -Quiet)) {
            throw "the preset role does not reach GPUI, so a preset could not draw"
        }
        foreach ($forbidden in @("wasmtime", "zup-windows", "zup-installer")) {
            if ($graph | Select-String -Pattern "(^|[^a-z-])$forbidden v" -Quiet) {
                throw "the preset role reaches $forbidden, which decides what an installation does"
            }
        }
    }
    finally { Pop-Location }

    # The facade, with the plugin role.
    Push-Location (Join-Path $work "zup-sdk")
    try {
        cargo test --all-targets --no-default-features --features plugin
        if ($LASTEXITCODE -ne 0) { throw "the plugin role does not build outside this repository" }
        $graph = cargo tree --edges normal --prefix none --no-default-features --features plugin
        if ($LASTEXITCODE -ne 0) { throw "could not read the plugin graph" }
        foreach ($name in $internal) {
            if ($graph | Select-String -Pattern "(^|[^a-z-])$name v" -Quiet) {
                throw "the plugin role reaches the internal crate $name"
            }
        }
        foreach ($expected in @("zup-plugin-sdk", "zup-plugin-abi")) {
            if (-not ($graph | Select-String -Pattern "(^|[^a-z-])$expected v" -Quiet)) {
                throw "the plugin role does not link $expected"
            }
        }
        # The two assertions the design is made of: a plugin carries no window,
        # and no runtime that would execute it.
        foreach ($forbidden in @("gpui-kit", "gpui-pre", "wasmtime")) {
            if ($graph | Select-String -Pattern "(^|[^a-z-])$forbidden v" -Quiet) {
                throw "the plugin role reaches $forbidden, which it must never carry"
            }
        }
    }
    finally { Pop-Location }

    ""
    "authoring surface: every published crate packages on its own, no published"
    "manifest names an internal crate, and each role of zup-sdk builds and tests"
    "from outside this workspace with only the graphs its role should have"
}
finally {
    Pop-Location
}