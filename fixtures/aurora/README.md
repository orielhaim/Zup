# Aurora

A preset for testing `zup ui pack` and `zup ui inspect`.

It is a real preset: it depends only on `zup-ui-sdk` and its `Settings` type is
ordinary Rust with a generated JSON Schema, so the package it produces is the same
shape a third-party preset produces.

```bash
zup ui pack --manifest fixtures/aurora --build x86_64-pc-windows-msvc
zup ui inspect fixtures/aurora/aurora-*.zupui
```
