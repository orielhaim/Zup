# Components

A component is a choice the person installing makes. Components become
checkboxes in the window, and resources tagged with a component are written only
when it is selected.

```toml
[[components]]
id = "program"
name = "Acme"
description = "The program itself."
required = true
default = true

[[components]]
id = "docs"
name = "Documentation"
description = "Notes and reference material."
default = false

[[components]]
id = "samples"
name = "Sample data"
description = "Files to try the program out with."
default = true
```

| Key | Default | Meaning |
| --- | --- | --- |
| `id` | required | A stable identifier |
| `name` | required | The label shown |
| `description` | no | One line under the label |
| `required` | `false` | Cannot be turned off |
| `default` | `true` | Selected when the person does not choose |
| `requires` | empty | Components that must also be selected |
| `group` | none | The [component group](#groups) this belongs to |
| `targets` | empty | Only offer it for these target profiles |

`default = true` is the default, so an optional component starts **on** unless
you say `default = false`. A required component cannot also default to off -
`required = true` with `default = false` is refused.

## Attaching resources

Tag any file mapping, launcher, `PATH` entry or service with a component:

```toml
[[files]]
source = "docs/**"
destination = "${install}/docs"
component = "docs"

[[launchers]]
location = "menu"
name = "Acme"
target = "${install}/acme.exe"
component = "program"
```

The `program` tag on that launcher is redundant - a resource with no component is
always written. It is worth writing anyway when the component exists, because it
says what the shortcut belongs to.

A component that no resource references still appears in the window. That is
legitimate when the component's effect is produced by a
[plugin](/advanced/plugins) or already present on the machine.

## Dependencies

```toml
[[components]]
id = "editor"
name = "Editor"
requires = ["core"]
```

`requires` names components that must be selected for this one to be available.
Zup refuses an unknown id, a component that depends on itself, and any cycle,
naming the cycle:

```text
component dependency cycle: editor → plugins → editor
```

## Groups

A group is a set of components chosen together. Membership lives on each
component, and a component with no group belongs to an implicit default group, so
a package that never mentions groups still has one coherent set.

```toml
[[component_groups]]
id = "editors"
label = "Editors"
description = "Optional editor integrations."
prominence = "secondary"

[[components]]
id = "vim"
name = "Vim integration"
group = "editors"
```

| Key | Default | Meaning |
| --- | --- | --- |
| `id` | required | A stable identifier |
| `label` | no | The group heading. Defaults to the id |
| `description` | no | One line under the heading |
| `prominence` | `auto` | How prominently to present the group |
| `selection` | `defaulted` | Whether the declared defaults are enough to install |

`prominence`:

| Value | Effect |
| --- | --- |
| `auto` | The conservative default, described below |
| `primary` | The person should see this decision before installing |
| `secondary` | A sensible default; the group can stay out of the happy path |

`auto` is conservative: a group with nothing optional to choose disappears, a
group that requires an explicit selection is primary, and everything else is
secondary.

`selection`:

| Value | Effect |
| --- | --- |
| `defaulted` | The declared defaults are a valid choice. The default |
| `explicit` | At least one optional component in the group must be selected |

A group with no members is an error, as is a component naming a group that is
not declared.

## Trying it

```bash
zup plan --enable docs --disable samples
```

`--enable` and `--disable` are repeatable, and take component ids. They change
the plan for one command without touching the manifest - which is the fastest
way to check that a component's `default` is the one you meant.

## Restricting to targets

```toml
[[components]]
id = "x64-symbols"
name = "Debug symbols (x64)"
targets = ["x64"]
```

An empty list means every profile. A component that is not offered for the
selected target does not exist, so a `requires` that names it is an error for
that target.

Next: [prerequisites](/configure/prerequisites).
