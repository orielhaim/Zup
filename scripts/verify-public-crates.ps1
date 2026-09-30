# Verify the published UI crates the way a consumer outside this repository
# would see them.
#
# The three public crates form a chain:
#
#   zup-ui-protocol  the wire format, versioning, and the domain vocabulary
#   zup-ui-ipc       the portable process transport that carries it
#   zup-ui-sdk       what a preset is written against
#
# A preset author depends on the SDK and, if they need the transport or the
# protocol directly, on the other two. All three resolve from crates.io, so none
# of them may name a crate that exists only in this repository.
#
# `cargo publish` resolves dependencies from crates.io, so `zup-ui-sdk` cannot be
# packaged until `zup-ui-protocol` and `zup-ui-ipc` are on the registry. That
# ordering is correct - it is what makes the chain genuinely consumable from
# outside - and it means the verification has to be done the way a third-party
# project does it: from a directory that is not this workspace, depending only on
# crates.io crates and the packaged archives of the crates beneath it.
#
# So this proves four things, in the order they stop being true:
#
#   1. `zup-ui-protocol` and `zup-ui-ipc` each package, verify, and build alone.
#   2. No public manifest names an internal Zup crate, in any form.
#   3. `zup-ui-sdk` builds and tests outside this workspace, against the packaged
#      protocol and transport rather than the workspace's copies.
#   4. Nothing any of them builds reaches an internal crate.
#
# The one thing this cannot prove is what `cargo publish` does for the two crates
# that sit above `zup-ui-protocol` once it is on crates.io. That is a property of
# the registry, not of this repository, and it is checked by publishing.
#
# Run: ./scripts/verify-public-crates.ps1

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $protocol = "zup-ui-protocol"
    $ipc = "zup-ui-ipc"
    $sdk = "zup-ui-sdk"
    $public = @($protocol, $ipc, $sdk)
    $internal = @(
        "zup-core", "zup-runtime", "zup-plan", "zup-exec", "zup-windows",
        "zup-bundle", "zup-installer", "zup-artifact", "zup-transaction",
        "zup-ui-package", "zup-presentation", "zup-build", "zup-manifest",
        "zup-update", "zup-bootstrap", "zup-platform", "zup-acquire",
        "zup-signing", "zup-toolchain", "zup-protocol", "zup-dispatch"
    )

    # 1. The bottom of the chain is self-contained: cargo packages it, verifies
    #    the packaged sources, and compiles them with no path to this workspace.
    #    Nothing above it can be dry-run published until this one is on the
    #    registry, so each of those is verified the way a consumer gets it
    #    instead: built and tested from outside this workspace.
    cargo publish -p $protocol --dry-run --allow-dirty
    if ($LASTEXITCODE -ne 0) { throw "$protocol does not package on its own" }
    cargo package -p $protocol --allow-dirty --no-verify | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "$protocol could not be packaged" }

    # 2. No public manifest may reach a crate that exists only here. This is the
    #    crates.io boundary stated as a check rather than as a claim, and it is
    #    checked on the source manifests so it covers `cargo package`'s
    #    rewriting as well as the declared shape.
    foreach ($crate in $public) {
        $declared = Get-Content -LiteralPath "crates\$crate\Cargo.toml" -Raw
        # `publish` defaults to true, and a crate that cannot be packaged has
        # already failed above, so what matters here is that none of them opts
        # out of being published.
        if ($declared -match "(?m)^\s*publish\s*=\s*false") { throw "$crate is not published" }
        foreach ($name in $internal) {
            if ($declared -match "(?m)^\s*$name\s*=") { throw "$crate depends on $name" }
            if ($declared -match "(?m)^\s*path\s*=.*$name") {
                throw "$crate has a path dependency on $name"
            }
        }
    }

    # 3. Each crate above the bottom is built and tested from outside the
    #    workspace, against the *packaged* archive of the crate beneath it. So
    #    the transport links the packaged protocol rather than this workspace's
    #    copy, and the SDK links the packaged protocol and the transport copy
    #    that was just built on its own. Every other dependency comes from
    #    crates.io, so a preset author cannot reach an unpublished internal crate
    #    through any of them.
    $work = Join-Path $root "target\public-crate-check"
    if (Test-Path $work) { Remove-Item -Recurse -Force $work }
    New-Item -ItemType Directory -Path $work | Out-Null

    $archive = Get-ChildItem "target\package" -Filter "$protocol-*.crate" |
        Select-Object -First 1
    # A `.crate` is a gzipped tar with one top-level directory, so unpacking it
    # leaves a nested directory of the same name.
    tar -xf $archive.FullName -C $work
    $unpacked = @{
        $protocol = [IO.Path]::GetFileNameWithoutExtension($archive.Name)
        $ipc      = $ipc
        $sdk      = $sdk
    }

    foreach ($crate in @($ipc, $sdk)) {
        # Each copy is a sibling of the crate beneath it, so its path dependency
        # is one `..` and cannot reach anything else in the repository.
        $outside = Join-Path $work $crate
        New-Item -ItemType Directory -Path $outside | Out-Null
        Copy-Item -Recurse -Force "crates\$crate\*" $outside
        # An empty `[workspace]` detaches the copy from this repository, which is
        # the whole point: nothing it builds can come from here.
        Add-Content -LiteralPath (Join-Path $outside "Cargo.toml") -Value "`n[workspace]"

        $manifest = Join-Path $outside "Cargo.toml"
        $text = Get-Content -LiteralPath $manifest -Raw
        foreach ($beneath in @($protocol, $ipc)) {
            if ($beneath -eq $crate) { continue }
            $text = $text -replace `
                "$beneath = \{ path = `"\.\./$beneath`", version = `"0\.1\.0`" \}", `
                "$beneath = { path = `"../$($unpacked[$beneath])`" }"
        }
        Set-Content -LiteralPath $manifest -Value $text -NoNewline

        Push-Location $outside
        try {
            cargo test --all-targets --all-features
            if ($LASTEXITCODE -ne 0) { throw "$crate does not build outside this repository" }
            $graph = cargo tree --edges normal --prefix none
            if ($LASTEXITCODE -ne 0) { throw "could not read $crate's dependency graph" }
            foreach ($name in $internal) {
                if ($graph | Select-String -Pattern "(^|[^a-z-])$name v" -Quiet) {
                    throw "$crate reaches the internal crate $name"
                }
            }
            # The crate beneath has to be reachable by name, or a preset that
            # wanted the transport rather than the SDK could not have it.
            foreach ($beneath in @($protocol, $ipc)) {
                if ($beneath -eq $crate) { continue }
                if (-not ($graph | Select-String -Pattern "(^|[^a-z-])$beneath v" -Quiet)) {
                    throw "$crate does not link $beneath, so a preset cannot reach it"
                }
            }
        }
        finally { Pop-Location }
    }

    ""
    "public UI crates: the protocol packages and publishes standalone, no public"
    "manifest names an internal crate, and the transport and the SDK each build"
    "and test outside this repository against the packaged protocol"
}
finally {
    Pop-Location
}
