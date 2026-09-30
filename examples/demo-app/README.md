# Demo App

A small, complete Zup application, for driving by hand.

It is not a fixture and nothing about it is special. It is an ordinary
application project in the same format a project outside this repository uses, read
by the same reader, built by the same build, presented by the same window. If a
command works here it works in a real project, and if something looks wrong here
it is wrong for everyone.

```
zup preview        open the installer window over a simulated machine
zup check          validate this project and everything it will ship
zup plan           print what installing it would change, without changing it
zup build          write "Demo App-Setup.exe" beside zup.toml
```

## Before the first run

`zup preview` and `zup build` need the binaries Zup composes an installer from.
Build them once:

```
cargo xtask toolchain build
```

That stages the runtime templates, the launchers, and the default preset. It is
the one build outside this directory, and it is shared by every project.

## What is in here

```
zup.toml                  the application
payload/
  demo.cmd                the program
  NOTES.txt               a file the repair flow can restore
  branding/logo.svg       installed artwork
  docs/README.txt         installed only by the "Documentation" component
  samples/greeting.txt    installed only by the "Sample data" component
```

The manifest is commented, and the comments say why each thing is there rather
than restating what the line already says. It is worth reading: it is a small but
complete tour of the format.

One required component and two optional ones, `user` or `machine` scope, a Start
menu entry, a desktop shortcut, and one directory on `PATH`. Those last three are
real system changes, which is the point — they are what "Show what will change"
lists, and what an uninstall has to take back out.

## `zup preview`

The window is the preset Zup ships, because the manifest names no package. The
first thing the window shows is this application's own name, version, publisher,
description and components, because the simulated machine is derived from this
application rather than from a demonstration.

It is safe to leave open. A preview writes nothing outside its own `.zup/`
directory: no application file, no `PATH` entry, no shortcut, no registry value,
no uninstall record, and no elevation. A control causes the event an engine would
have caused and the state machine decides what it means, so every state the window
can reach is a state a real installation can reach.

Things worth trying:

- **Edit `hero` in `zup.toml` while it is open.** The line under the product name
  changes without the window restarting, and without a build. That is the fastest
  proof that a data change and a code change are different things.
- **Edit it into something invalid** — `hero = 42`. The session reports the
  schema's own refusal, naming the setting, and keeps showing the last settings
  that fitted. A half-finished edit must not cost a working window.
- **Press the buttons.** Install, Stop, and the state changes are all real
  behaviour. There is no engine behind them, so an operation waits at "Preparing"
  until you drive it from the console — see below.
- **Ctrl+C.** No child process is left behind.

The controls are typed into the session, not clicked:

```
install | maintenance        which surface this machine shows
user | machine              who the installation is for
components <layout>         none | one-optional | many | required-and-optional
run                         start an installation
next                        step the running installation along
blocked <text>              a file in the way
rollback                    the transaction failed and was undone
recovery                    the last transaction did not finish
reboot                      the machine must restart
busy                        another process is already running one
checking <text>             the update channel is being asked
up-to-date <version>        the channel has nothing newer
available <version>         the channel has a newer release
update-failed <text>        the channel could not be reached
drift <a,b>                 a repair found these resources changed
quit                        stop the preview
```

## `zup build`, and then really installing it

```
zup build
```

Writes `Demo App-Setup.exe` and `zup-release.json` here. The executable is about
90 MiB, almost all of it the installer runtime and the preset; the payload is
2.7 KiB.

Run the installer, and work through:

- **Scope.** This application offers `user` and `machine`. Machine scope needs
  elevation; a shell that is not elevated will be told so, which is the honest
  answer rather than a failure.
- **Components.** `Demo App` is required; `Documentation` starts off; `Sample data`
  starts on. Press "Show what will change" — the docs and samples files appear and
  disappear with their checkboxes.
- **Install.** Then find it in the Start menu, or run `demo.cmd` from the install
  directory, or open a new command prompt and run `demo --version`, which only
  works if the `PATH` entry was taken.
- **Change** on the maintenance surface re-opens the component choices.
- **Repair.** Delete a file from the install directory — `NOTES.txt` is the easiest
  — then press Repair. It comes back.
- **Uninstall.** Everything goes, including the shortcut and the `PATH` entry.
  `demo --version` stops working, which is how you know.

Where the installer puts things is in the `Location` line of the window, or in:

```
zup plan --scope user
```

## Updating it

Change `version` in `zup.toml` and run `zup build` again, then run the new
installer over the old installation. That is the modify/replace path rather than
an update-from-a-channel path: `[updates]` is not configured here, because it
needs a real repository, channel and trusted root, and a demo with three invented
values would fail at the point a real application would succeed.

The update *buttons* are still there in the maintenance window, and `zup preview`
can drive them from the console:

```
available 1.1.0
up-to-date 1.0.0
update-failed no route to host
```

## The logo, and what a UI asset actually is

`payload/branding/logo.svg` is a file this application **ships**: the installer
places it, a plan lists it, a repair restores it and an uninstall removes it.

It is not a *UI asset*, which is a different thing. A UI asset is a file the
**window itself reads** — a logo the installer draws in its own chrome. It only
becomes one when the selected preset's settings schema marks it, so that the
application can say `logo = "branding/logo.svg"` under `[ui.settings]` and have
Zup resolve, hash and hand over the bytes.

The bundled preset takes exactly one setting, `hero`, and marks nothing as an
asset. `zup ui inspect` on a packed copy of it says so:

```
preset        zup-preset-default 0.0.1
settings      hero (others permitted)
```

"others permitted" is the honest detail: naming a `logo` setting anyway would
validate and then be ignored, because the preset would never read it. This project
ships the logo as a payload file instead, which does something.

To see a real UI asset, point this project at a preset that takes one. In a
preset's own settings type:

```rust
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Settings {
    hero: Option<String>,
    #[schemars(extend("x-zup-asset" = true))]
    logo: Option<zup_ui_sdk::AssetRef>,
}
```

`zup ui pack` that preset, then set in `zup.toml`:

```toml
[ui]
preset = "./aurora.zupui"
```

and `zup preview` will resolve the file, hash it, materialize it into the
session's own directory and hand the preset a logical name and verified content —
never a path into this project. `zup preview` also re-resolves when you edit the
package, and only replaces the window once the new one has connected.

## Cleaning up

```
zup build --force
```

replaces the installer. To remove everything the demo installed, run the
installer's own Uninstall from the maintenance window — that is the path worth
exercising, and it is the one that has to take the `PATH` entry back out.
