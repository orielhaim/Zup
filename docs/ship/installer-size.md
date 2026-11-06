# Installer size

Installer size includes the runtime, compressed payload, plugins, and any GUI preset. Console and Headless omit GPUI; a GUI preset adds its own native executable and assets.

Measure your application with release templates rather than relying on sizes from another project:

```sh
cargo xtask toolchain build --profile installer
cargo build -p zup --bin zup
python scripts/ci/installer-size.py --cli target/debug/zup --toolchain target/installer/toolchain/0.0.1 --output target/sizes
```

Use `zup.exe` on Windows. Build each frontend separately when comparing sizes, so Cargo feature unification does not mix their dependencies. Measurements change with the payload, target, compiler, and dependencies.

See [artifacts](artifacts.md) for packaging options and [frontends](/guide/frontends) for UI choices.
