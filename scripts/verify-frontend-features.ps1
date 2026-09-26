$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    # Each frontend must compile alone, not only alongside the build feature.
    foreach ($feature in @("headless", "console", "gui", "build")) {
        cargo check -p zup --no-default-features --features $feature --all-targets
    }

    $headless = cargo tree -p zup --no-default-features --features headless --edges normal --prefix none
    if ($headless -match "zup-ui|gpui-kit|cliclack|indicatif|^console v") {
        throw "headless dependency graph contains an interactive UI crate"
    }
    $console = cargo tree -p zup --no-default-features --features console --edges normal --prefix none
    if ($console -match "zup-ui|gpui-kit") {
        throw "console dependency graph contains GPUI"
    }
    cargo build -p zup --features build --bin zup --release
    cargo build -p zup --no-default-features --features build,gui --bin zup-setup-gui --release
    cargo build -p zup --no-default-features --features build,console --bin zup-setup-console --release
    cargo build -p zup --no-default-features --features build,headless --bin zup-setup-headless --release
    $runtimes = @(
        "target/release/zup-setup-gui.exe",
        "target/release/zup-setup-console.exe",
        "target/release/zup-setup-headless.exe"
    )
    $objdump = Get-Command objdump -ErrorAction SilentlyContinue
    foreach ($runtime in $runtimes) {
        $item = Get-Item -LiteralPath $runtime
        if ($null -eq $objdump) {
            "{0} {1:N1} MiB" -f $item.Name, ($item.Length / 1MB)
            continue
        }
        $subsystem = @(objdump -p $item.FullName | Select-String "Subsystem" | Where-Object { $_.Line -notmatch "SubsystemVersion" })[0].Line.Trim()
        "{0} {1:N1} MiB {2}" -f $item.Name, ($item.Length / 1MB), $subsystem
    }
}
finally {
    Pop-Location
}
