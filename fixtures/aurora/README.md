# Aurora

A preset for testing `zup preset pack` and `zup preset inspect`.

It is a real preset: it depends only on `zup-ui-sdk` and its `Settings` type is
ordinary Rust with a generated JSON Schema, so the package it produces is the same
shape a third-party preset produces.

```bash
zup preset pack --manifest fixtures/aurora --build x86_64-pc-windows-msvc
zup preset inspect fixtures/aurora/aurora-*.zupui
```
