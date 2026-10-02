# Application lifecycle

The installer is not a one-shot file copier. A Zup package defines the application over its installed lifetime.

## Install

A fresh installer resolves the selected scope, components, install directory, prerequisites and owned resources, then installs that selection.

The preset presents the choices. The project defines what those choices mean.

## Upgrade

Running a newer package against an existing installation upgrades it. The new project version becomes the source of truth for the resources Zup owns.

A lower version is not accepted as an update.

## Modify

Optional components can be changed after installation. A maintenance UI sends `Modify` after the user changes the component selection.

Resources attached to removed components leave the owned selection; resources attached to newly selected components enter it.

## Repair

Repair checks resources Zup owns and restores managed resources that no longer match the installed state where repair is safe.

Use repair for installation drift. Do not use it as a generic reset mechanism for user-owned data.

## Update

When `[updates]` is configured, the maintenance UI can check the configured channel and move to a newer release.

Update policy belongs to the release configuration, not to the preset.

## Uninstall

Uninstall removes resources Zup still owns. A preset requests uninstall, presents confirmation, and renders progress; it does not implement cleanup itself.

This ownership boundary is why files, services, PATH entries and plugin-produced resources should be declared rather than hidden in install scripts.

## One model, different surfaces

Fresh installation and maintenance use the same project model. The UI changes because the available actions change:

```text
fresh install   → choose → install
installed app   → modify / repair / update / uninstall
```

See [Presets](/presets/) for presenting those states and [Updates](/ship/updates) for release-channel configuration.
