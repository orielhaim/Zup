# Aurora

A preset for testing `zup preset pack` and `zup preset inspect`.

It is a real preset: its only dependency is `zup-sdk`, and its `Settings` type
is annotated `#[zup_sdk::preset::settings]`, so the schema it packs is the one
Zup generates from the preset's own Rust type. The package it produces is
therefore the same shape a third-party preset produces.

```bash
zup preset pack --manifest fixtures/aurora --build x86_64-pc-windows-msvc
zup preset inspect fixtures/aurora/aurora-*.zupui
```

Its type lives in `src/lib.rs` so a test can name it without building the
binary; `src/main.rs` is only the entry point.