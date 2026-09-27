$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    # The dispatcher has to start on the narrowest machine any variant can serve,
    # which on Windows is always 32-bit x86: it runs natively on x86, under
    # WOW64 on x64, and under the x86 compatibility layer on arm64. It is also
    # the smallest of the three, which matters for a file that is downloaded
    # before any of it has been needed.
    $target = "i686-pc-windows-msvc"

    # Size is a design constraint for this package, so it is built with the
    # release profile's size settings. `-C lto` must not reach RUSTFLAGS: it
    # conflicts with the bitcode settings LTO needs.
    $env:CARGO_PROFILE_RELEASE_LTO = "fat"
    $env:CARGO_PROFILE_RELEASE_OPT_LEVEL = "z"
    $env:CARGO_PROFILE_RELEASE_PANIC = "abort"
    $env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS = 1
    cargo build -p zup-dispatch --target $target --release --bins
    cargo build -p zup-dispatch --target $target --bins
    Remove-Item Env:CARGO_PROFILE_RELEASE_LTO
    Remove-Item Env:CARGO_PROFILE_RELEASE_OPT_LEVEL
    Remove-Item Env:CARGO_PROFILE_RELEASE_PANIC
    Remove-Item Env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS

    # Both profiles are installed beside the `zup` executable, which is where
    # `zup build` looks for a template and where the tests look for one.
    foreach ($profile in @("debug", "release")) {
        $destination = "target/$profile"
        foreach ($name in @("zup-dispatch", "zup-dispatch-console")) {
            $built = "target/$target/$profile/$name.exe"
            $item = Get-Item -LiteralPath $built
            Copy-Item -LiteralPath $built -Destination "$destination/$name.exe" -Force
            "{0} {1} {2:N0} bytes" -f $profile, $item.Name, $item.Length
        }
    }
}
finally {
    Pop-Location
}
