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

    # Each flavour is built and installed before the next one starts, because they
    # share an output path and the second build would otherwise overwrite the
    # first. Both are installed beside the `zup` executable, which is where
    # `zup build` looks for a template and where the tests look for one.
    #
    # The machine is in the installed name: `cargo build` writes the unsuffixed
    # name for the host, and a host image silently replacing the x86 one would
    # make every composition test fail on a width rule instead of on what it
    # tests. The flavour is in the name for the same reason — an online image and
    # an offline one differ by three megabytes, and a test that measured the
    # wrong one would report a number nobody could reproduce.
    $flavours = @(
        @{ Feature = "";         Suffix = "" },
        @{ Feature = "online";  Suffix = "-online" }
    )
    $binaries = @("zup-dispatch", "zup-dispatch-console")

    function Install-Images([string]$profile, [string]$suffix) {
        $destination = "target/$profile"
        foreach ($name in $binaries) {
            $built = "target/$target/$profile/$name.exe"
            $item = Get-Item -LiteralPath $built
            $installed = "$name$suffix-$target.exe"
            Copy-Item -LiteralPath $built -Destination "$destination/$installed" -Force
            "{0,-8} {1,-28} {2,12:N0} bytes" -f $profile, $installed, $item.Length
        }
    }

    foreach ($flavour in $flavours) {
        $features = @()
        if ($flavour.Feature) {
            $features = @("--features", $flavour.Feature)
        }
        cargo build -p zup-dispatch --target $target @features --release --bins
        Install-Images "release" $flavour.Suffix
        cargo build -p zup-dispatch --target $target @features --bins
        Install-Images "debug" $flavour.Suffix
    }

    Remove-Item Env:CARGO_PROFILE_RELEASE_LTO
    Remove-Item Env:CARGO_PROFILE_RELEASE_OPT_LEVEL
    Remove-Item Env:CARGO_PROFILE_RELEASE_PANIC
    Remove-Item Env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS
}
finally {
    Pop-Location
}
