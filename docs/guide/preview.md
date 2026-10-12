# Check and preview

| Command | Question |
| --- | --- |
| `zup check` | Is the project valid? |
| `zup doctor` | Can this machine build the selected targets now? |
| `zup plan` | What would the selected installation change? |
| `zup preview` | What will the installer window present? |

```bash
zup check
zup doctor
zup plan --scope user
zup preview
```

`check` validates the project. `doctor` is read-only build readiness. `plan` shows owned files and resources without installing (`--enable` / `--disable` reproduce component choices). `preview` runs the resolved preset against a simulated machine without writing application files or system state. Preset authors use [`zup preset dev`](/presets/develop) instead.
