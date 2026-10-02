# Help

Zup reports failures with a diagnostic code, a message, and usually the location
in your files. The code is stable; the message is written for people and will be
reworded.

Every operation command can produce the same diagnostics as a machine-readable
document without writing anything:

```bash
zup check --format json
zup doctor --format json
```

## Troubleshooting

[Troubleshooting](./troubleshooting) covers the failures that come up most
often: a missing toolchain, a refused target, a manifest key that is not
accepted, a signing plan that will not verify, and a publish that refuses.

## Reading a diagnostic

```json
{
  "severity": "error",
  "code": "zup_manifest::missing_install_directory",
  "message": "install directory for scope `user` is not specified",
  "source": { "file": "zup.toml", "start_line": 21, "start_column": 1 },
  "help": "set [install.directory] `user` and/or `machine` to cover the configured scope"
}
```

`severity` is `error`, `warning` or `notice`. `source` is optional; line and
column are 1-based.

See [Automation output](/reference/automation) for the full document shape and the
exit-code table.