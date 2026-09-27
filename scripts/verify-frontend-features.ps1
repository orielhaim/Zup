$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    # Every runtime frontend must build on its own, with no authoring plane
    # behind it.
    $frontends = @("gui", "console", "headless")
    foreach ($feature in $frontends) {
        cargo check -p zup-installer --no-default-features --features $feature --all-targets
        if ($LASTEXITCODE -ne 0) { throw "the $feature runtime does not build on its own" }
    }

    # A Cargo feature must not choose which executable somebody gets. `zup` has no
    # features, and the runtime's only features are its three presentations, so
    # `cargo run`, `cargo build`, and `cargo install` produce the same product with
    # no flag deciding otherwise.
    $featureRules = @{
        "crates/zup/Cargo.toml" = @()
        "crates/zup-installer/Cargo.toml" = @("gui", "console", "headless")
    }
    foreach ($entry in $featureRules.GetEnumerator()) {
        $manifest = Get-Content -LiteralPath $entry.Key -Raw
        $declared = @()
        if ($manifest -match '(?ms)^\[features\](.*?)(?=^\[|\z)') {
            $declared = $Matches[1] -split "`n" |
                ForEach-Object { $_.Trim() } |
                Where-Object { $_ -match '^[A-Za-z0-9_-]+\s*=' } |
                ForEach-Object { ($_ -split '=')[0].Trim() }
        }
        $unexpected = @($declared | Where-Object {
            $_ -ne "default" -and $entry.Value -notcontains $_
        })
        if ($unexpected.Count -gt 0) {
            throw "$($entry.Key) declares features that select a role: $($unexpected -join ', ')"
        }
        foreach ($expected in $entry.Value) {
            if ($declared -notcontains $expected) {
                throw "$($entry.Key) is missing the `$expected` feature"
            }
        }
        # An empty `default` is the same as none, and a non-empty one would turn
        # a plain `cargo build` into a choice.
        if (($declared -contains "default") -and
            $manifest -notmatch '(?m)^default\s*=\s*\[\s*\]') {
            throw "$($entry.Key) has a non-empty `default` feature"
        }
    }

    # The build plane must not be reachable from any runtime frontend. This is the
    # rule the whole package split exists to enforce, and a check that only
    # compiled would let a dependency creep back in silently.
    #
    # These are zup's own packages and crates. A third-party crate that happens to
    # depend on one of them is not this rule's business; the GUI stack pulls
    # `schemars` and `toml_edit` transitively through gpui-kit and always has, and
    # a check that flagged that would be a check nobody could satisfy.
    $forbidden = @(
        "zup-build", "zup-manifest", "zup-plugin-build", "zup-publish",
        "zup-publish-github", "zup-distribute-github", "clap_complete",
        "inquire", "zup-xtask", "zup", "zup-toolchain"
    )
    foreach ($feature in $frontends) {
        $tree = cargo tree -p zup-installer --no-default-features --features $feature --edges normal --prefix none
        if ($LASTEXITCODE -ne 0) { throw "could not read the $feature dependency graph" }
        foreach ($name in $forbidden) {
            # `zup ` with a space matches the developer CLI's own line and no
            # other crate's, which is what distinguishes it from `zup-core` and
            # the rest of the family in a prefix-stripped tree.
            $hit = $tree | Select-String -Pattern "(^|[^a-z-])$name " -Quiet
            if ($hit) { throw "the $feature runtime depends on $name" }
        }
    }

    # The headless frontend is a pipeline, not a window: it must contain neither
    # the GPU stack nor any terminal presentation.
    $headless = cargo tree -p zup-installer --no-default-features --features headless --edges normal --prefix none
    foreach ($pattern in @("^zup-ui v", "^gpui-kit v", "^cliclack v", "^indicatif v", "^console v")) {
        if ($headless | Select-String -Pattern $pattern -Quiet) {
            throw "the headless runtime depends on $pattern"
        }
    }

    # The console frontend is a terminal. It must not contain the GPU stack.
    $console = cargo tree -p zup-installer --no-default-features --features console --edges normal --prefix none
    foreach ($pattern in @("^zup-ui v", "^gpui-kit v")) {
        if ($console | Select-String -Pattern $pattern -Quiet) {
            throw "the console runtime depends on $pattern"
        }
    }

    # The developer CLI is the other half of the rule: it must contain no runtime
    # presentation stack, and it must not depend on the runtime it produces. The
    # engine crates it does reach for - `zup-windows` for the artifact backend,
    # `zup-plan` for `zup plan` - are the developer's, and are not this rule.
    $developer = cargo tree -p zup --edges normal --prefix none
    foreach ($pattern in @("^zup-ui v", "^gpui-kit v", "^cliclack v", "^indicatif v", "^console v", "^zup-installer v")) {
        if ($developer | Select-String -Pattern $pattern -Quiet) {
            throw "the developer CLI depends on $pattern"
        }
    }

    # Size is the number this split exists to improve, so measure it rather than
    # assume it. The report is not a gate: a change in it is information, and the
    # gate is what the checks above already decided.
    cargo build -p zup --bin zup --release
    foreach ($feature in $frontends) {
        $binary = switch ($feature) {
            "gui" { "zup-setup-gui" }
            "console" { "zup-setup-console" }
            "headless" { "zup-setup-headless" }
        }
        cargo build -p zup-installer --no-default-features --features $feature --bin $binary --release
        if ($LASTEXITCODE -ne 0) { throw "the $feature release runtime does not build" }
    }

    ""
    "release sizes"
    $images = @("zup/zup") + ($frontends | ForEach-Object {
        switch ($_) {
            "gui" { "zup-installer/zup-setup-gui" }
            "console" { "zup-installer/zup-setup-console" }
            "headless" { "zup-installer/zup-setup-headless" }
        }
    })
    foreach ($entry in $images) {
        $path = "target/release/$($entry.Split('/')[1]).exe"
        if (-not (Test-Path -LiteralPath $path)) { continue }
        $item = Get-Item -LiteralPath $path
        "{0,-22} {1,10:N1} MiB" -f $entry, ($item.Length / 1MB)
    }
}
finally {
    Pop-Location
}
