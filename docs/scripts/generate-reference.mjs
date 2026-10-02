/**
 * Generate the per-section key tables in `docs/reference/zup-toml.md` from
 * `schema/zup.schema.json`.
 *
 * The prose in that page is written by hand. Only the tables are generated,
 * because a table of keys, types, defaults and requiredness is exactly the thing
 * that goes stale silently, and exactly the thing the schema already knows.
 *
 * Run after any change to the manifest model:
 *
 *   cargo run -p zup -- schema --output schema/zup.schema.json
 *   node docs/scripts/generate-reference.mjs
 *
 * The page carries `<!-- generated:<name> -->` markers. Text between a marker and
 * the next `<!-- /generated -->` is replaced; everything else is left alone. A
 * marker with no entry in `sections` is reported, because a table that silently
 * stops regenerating is worse than one that fails to build.
 */

import { readFile, writeFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const here = dirname(fileURLToPath(import.meta.url))
const repo = join(here, '..', '..')
const schemaPath = join(repo, 'schema', 'zup.schema.json')
const pagePath = join(here, '..', 'reference', 'zup-toml.md')

/**
 * Rust type names are not the vocabulary a user writes a manifest in. The schema
 * names its definitions after the types that back them; these are the names a
 * reader sees instead. An unlisted name is a real concept with a
 * self-explaining name, and is used as written.
 */
const FRIENDLY = {
  // Scalars a user writes by hand.
  AppId: 'string',
  NonEmptyString: 'string',
  ProtocolScheme: 'string',
  ServiceId: 'string',
  ComponentId: 'string',
  FileAssociationId: 'string',
  TargetProfileId: 'string',
  PrerequisiteId: 'string',
  PluginId: 'string',
  RuntimeRequirementId: 'string',
  InstalledPackageId: 'string',
  ProjectPath: 'string',
  Sha256Digest: 'string',
  FileExtension: 'string',

  // Enumerations, written out because the value matters more than the type.
  Frontend: '`gui` \\| `console` \\| `headless`',
  InstallScope: '`user` \\| `machine` \\| `either`',
  ArtifactKind: '`universal` \\| `single`',
  ArtifactMode: '`offline` \\| `thin`',
  DistributionHost: '`static` \\| `github`',
  LauncherLocation: '`menu` \\| `desktop`',
  ServiceStart: '`automatic` \\| `manual` \\| `disabled`',
  ComponentProminence: '`auto` \\| `primary` \\| `secondary`',
  SelectionRequirement: '`defaulted` \\| `explicit`',
  PrerequisiteArchitecture: '`x86` \\| `x64` \\| `arm64` \\| `current` \\| `any`',
  Privilege: '`user` \\| `system`',

  // Nested tables. The sub-section of this page carries their own table.
  Install: 'table',
  InstallDirectory: 'table',
  TargetProfile: 'table',
  ArtifactProfile: 'table',
  Source: 'table',
  Github: 'table',
  GithubWorkflow: 'table',
  GithubTag: 'table',
  GithubNotes: 'table',
  PrerequisiteRequirement: 'table',
  PrerequisitePackage: 'table',
  PrerequisiteInstaller: 'table',

  // Concepts with a guide page.
  Template: '[template](/configure/install#path-templates)',
  IconSetting: '[icon](/configure/app#icon)',
  Condition: '[condition](/configure/files#conditions)',
}

/**
 * Keys the schema leaves undescribed, or describes with a sentence that runs into
 * a list the table cannot carry. Keyed `Declaration.key`, where `Declaration` is
 * the name the page uses - `app`, `components`, `files`, `launchers` - so the
 * wording survives the schema's `Targeted2`-style renumbering.
 */
const MEANING = {
  'SchemaApp.id': 'A reverse-DNS identifier. Stable across versions',
  'SchemaApp.name': 'The display name',
  'SchemaApp.version': 'A semver version',
  'SchemaApp.publisher': 'Shown in the window and in the maintenance tool',
  'SchemaApp.main': 'The file the platform launches, relative to the install directory',
  'SchemaApp.description': 'One line, shown under the name',
  'SchemaApp.icon': 'A path, or a table with `source` and `padding`',

  'Build.targets': 'The target matrix. At least one profile is required',
  'TargetProfile.target': 'A target triple',
  'TargetProfile.source': 'A table naming the directory the payload comes from',
  'TargetProfile.frontend': 'The installer experience. Inherits the top-level one',
  'TargetProfile.install': 'A full `[install]` table. Inherits the top-level one',
  'Source.directory': 'The directory this target’s payload comes from',

  'Install.scope': 'Who the application installs for',
  'Install.directory': 'A path template per scope',
  'Install.allow_directory_override': 'Let the person change the location in the window',

  'Ui.preset': 'A `.zupui` package inside the project. Absent means the preset Zup ships',
  'Ui.settings': 'Values for the chosen preset’s settings',

  'Updates.repository': 'The URL clients read the release graph from',
  'Updates.channel': 'The channel this release answers to',
  'Updates.root': 'A path, read at build time, to the TUF root clients trust',

  'Distribution.host': 'Where a client fetches release content from',

  'Github.draft': 'Leave the release a draft',
  'Github.prerelease': 'Mark the release a prerelease',
  'GithubNotes.policy': 'Where a release’s notes come from',
  'GithubWorkflow.sign': 'Commands that sign the composed artifacts',

  'prerequisites.description': 'One line under the label',
  'prerequisites.target': 'Which architecture to check for',
  'prerequisites.requirement': 'How Zup decides whether it is satisfied',
  'prerequisites.package': 'Where the installer comes from',
  'prerequisites.installer': 'How to run the package',
  'prerequisites.component': 'Only required when that component is selected',

  'components.description': 'One line under the label',
  'components.required': 'Cannot be turned off',
  'components.default': 'Selected when the person does not choose',
  'components.requires': 'Components that must also be selected',
  'components.group': 'The group this component belongs to',

  'component_groups.label': 'The group heading. Defaults to the id',
  'component_groups.description': 'One line under the heading',
  'component_groups.prominence': 'How prominently to present the group',
  'component_groups.selection': 'Whether the declared defaults are enough to install',

  'files.source': 'A glob pattern inside the target’s source directory',
  'files.destination': 'A template naming a directory',
  'files.allow_empty': 'Accept a pattern that matches nothing',

  'launchers.location': 'Where the shortcut is written',
  'launchers.name': 'The label shown',
  'launchers.target': 'A template for the executable',
  'launchers.arguments': 'Default empty',
  'launchers.working_directory': 'A template for the working directory',

  'path.value': 'A directory added to the search path',

  'services.display_name': 'The name shown in Services',
  'services.binary': 'A template for the executable',
  'services.arguments': 'Default empty',
  'services.start': 'When the service starts',

  'protocols.scheme': 'A URI scheme, such as `acme` in `acme://`',
  'protocols.executable': 'A template for the handler',
  'protocols.args': 'Default empty',

  'file_associations.extension': 'A bare file extension, such as `.acme`',
  'file_associations.description': 'The type name shown by Windows',
  'file_associations.executable': 'A template for the handler',

  'plugins.source': 'A project-relative path to the `.wasm`',

  'prerequisite.installer.arguments': 'Passed to the package. At most 128, and no templates',
  'prerequisite.installer.success_exit_codes': 'Exit codes that mean installed',
  'prerequisite.installer.reboot_exit_codes': 'Exit codes that mean installed, pending a reboot',
  'prerequisite.installer.privilege': 'Whether installing this needs elevation',
}

/**
 * Keys whose meaning is the same on whichever declaration carries them. Keyed
 * `Def.key` where that differs, bare `key` otherwise. `targets` is the flattened
 * `Targeted<T>` filter and appears on every array of tables, so a shared
 * wording is the correct one rather than a per-definition entry.
 */
const SHARED = {
  targets: 'Only apply to these target profiles',
  id: 'A stable identifier',
  name: 'The label shown',
}

/** Collapse a schema node to the short type name a reader would write. */
function typeOf(node, defs, depth = 0) {
  if (!node || depth > 6) return '-'
  if (node.const !== undefined) return `\`${node.const}\``
  if (node.enum) return node.enum.map((v) => `\`${v}\``).join(' \\| ')
  if (node.anyOf) {
    const parts = node.anyOf
      .filter((alt) => alt.type !== 'null')
      .map((alt) => typeOf(alt, defs, depth + 1))
    return [...new Set(parts)].join(' \\| ')
  }
  if (node.$ref) {
    const friendly = FRIENDLY[node.$ref.replace('#/$defs/', '')]
    if (friendly !== undefined) return friendly
    return `\`${node.$ref.replace('#/$defs/', '')}\``
  }
  if (node.type === 'array') return `array of ${typeOf(node.items, defs, depth + 1)}`
  if (Array.isArray(node.type)) {
    return node.type.filter((t) => t !== 'null').map((t) => `\`${t}\``).join(' \\| ')
  }
  switch (node.type) {
    case 'string':
    case 'integer':
    case 'number':
    case 'boolean':
      return `\`${node.type}\``
    case 'object':
      return 'table'
    default:
      return node.type ? `\`${node.type}\`` : '-'
  }
}

/** The default value rendered for a cell, or an em dash. */
function defaultOf(node) {
  if (!node) return '-'
  if (node.default !== undefined) return `\`${JSON.stringify(node.default)}\``
  // A nullable optional is an `Option<T>` in the model, so the branch's own
  // default applies.
  if (node.anyOf?.some((alt) => alt.type === 'null')) return '`null`'
  return '-'
}

/** Resolve a property through `$ref` and `anyOf` to something with a description. */
function describe(node, defs, depth = 0) {
  if (!node || depth > 4) return ''
  if (node.description) return node.description.trim()
  if (node.$ref && defs[node.$ref.replace('#/$defs/', '')]) {
    return describe(defs[node.$ref.replace('#/$defs/', '')], defs, depth + 1)
  }
  if (node.anyOf) {
    const parts = node.anyOf.map((alt) => describe(alt, defs, depth + 1)).filter(Boolean)
    return [...new Set(parts)].join(' ')
  }
  return ''
}

/** The first sentence, which is the part that belongs in a table. */
function firstSentence(text) {
  if (!text) return ''
  // A line ending in a colon introduces a list the table cannot carry, so the
  // lead-in is dropped rather than truncated into a dangling fragment.
  let sentence = (text.split('\n').find((l) => l.trim().length > 0) ?? '').trim()
  if (sentence.endsWith(':')) return ''
  const stop = sentence.search(/[.:](\s|$)/)
  if (stop > 0) sentence = sentence.slice(0, stop + 1)
  return sentence
}

function escapeCell(text) {
  return text.replace(/\|/g, '\\|').replace(/\n+/g, ' ').trim()
}

/**
 * @param label    the page's own name for this table (`components`, `files`, …),
 *                 used to look up MEANING. The schema's `Targeted2` numbering is
 *                 an artifact of how the generator walked the manifest, so
 *                 keying on it would silently change the wording of every
 *                 declaration the day a key is added.
 */
function table(defName, defs, label = defName) {
  const def = defs[defName]
  if (!def?.properties) throw new Error(`no \`properties\` on \`${defName}\` in the schema`)
  const required = new Set(def.required ?? [])
  const rows = Object.entries(def.properties).map(([key, node]) => {
    const summary =
      MEANING[`${label}.${key}`] ??
      (escapeCell(firstSentence(describe(node, defs))) || SHARED[key] || '')
    if (!summary) {
      throw new Error(`\`${label}.${key}\` (\`${defName}\`) has no meaning and no schema description`)
    }
    // Four columns rather than five: a `Required` column of yes/no is noise when
    // the answer is already implied, and a fifth column is what pushes these
    // tables into a horizontal scrollbar.
    return `| \`${key}\` | ${typeOf(node, defs)} | ${defaultOf(node)} | ${summary}${
      required.has(key) ? ' **Required.**' : ''
    } |`
  })
  return ['| Key | Type | Default | Meaning |', '| --- | --- | --- | --- |', ...rows].join(
    '\n',
  )
}

const schema = JSON.parse(await readFile(schemaPath, 'utf8'))
const defs = schema.$defs ?? {}

/**
 * The schema emits one `Targeted<T>` definition per array of tables and numbers
 * them in walk order, so the names mean nothing on their own. Read the mapping
 * out of the top-level properties rather than hardcoding it: adding a
 * declaration to the manifest then needs no change here.
 */
const targeted = {}
for (const [key, node] of Object.entries(schema.properties ?? {})) {
  const ref = node?.items?.$ref
  if (!ref) continue
  const name = ref.replace('#/$defs/', '')
  if (name.startsWith('Targeted')) targeted[name] = `[[${key}]]`
}

/** The schema definition name carrying the `[[key]]` array of tables. */
function defFor(key) {
  const wanted = `[[${key}]]`
  for (const [name, tomlKey] of Object.entries(targeted)) {
    if (tomlKey === wanted) return name
  }
  throw new Error(`no Targeted definition for \`${wanted}\` in the schema`)
}

const sections = {
  app: table('SchemaApp', defs),
  build: table('Build', defs),
  'build.targets': table('TargetProfile', defs),
  source: table('Source', defs),
  install: table('Install', defs),
  'install.directory': table('InstallDirectory', defs),
  'build.artifacts': table('ArtifactProfile', defs),
  ui: table('Ui', defs),
  updates: table('Updates', defs),
  distribution: table('Distribution', defs),
  publish: table('Publish', defs),
  'publish.github': table('Github', defs),
  'publish.github.tag': table('GithubTag', defs),
  'publish.github.notes': table('GithubNotes', defs),
  'publish.github.workflow': table('GithubWorkflow', defs),
  iconoptions: table('IconOptions', defs),
  component: table(defFor('components'), defs, 'components'),
  'component.group': table(defFor('component_groups'), defs, 'component_groups'),
  prerequisite: table(defFor('prerequisites'), defs, 'prerequisites'),
  'prerequisite.installer': table('PrerequisiteInstaller', defs, 'prerequisite.installer'),
  plugin: table(defFor('plugins'), defs, 'plugins'),
  files: table(defFor('files'), defs, 'files'),
  launchers: table(defFor('launchers'), defs, 'launchers'),
  path: table(defFor('path'), defs, 'path'),
  services: table(defFor('services'), defs, 'services'),
  protocols: table(defFor('protocols'), defs, 'protocols'),
  'file_associations': table(defFor('file_associations'), defs, 'file_associations'),

  // Enumerations and concept types read better as a short list than as a
  // one-row table, and the prose on this page already explains each.
  frontend: '- `gui` - a graphical installer window (default)\n- `console` - a console front end\n- `headless` - no interface, no prompts',
  'install.scope': '- `user` - the current user, no elevation\n- `machine` - everyone on the machine, elevation required\n- `either` - the person chooses in the window',
  'artifact.kind': '- `universal` - one file carrying every included target, selected at run time (default)\n- `single` - one file carrying exactly one target',
  'artifact.mode': '- `offline` - every required byte is inside the artifact; no network is used (default)\n- `thin` - the artifact carries what it needs to start, and fetches the rest by digest',
  'distribution.host': '- `static` - a plain file tree of `blobs/`, `releases/`, `metadata/` (default)\n- `github` - release assets, addressed by tag or by the host\'s stable alias',
  'notes.policy': '- `generated` - a summary Zup writes (default; `github` is accepted as a synonym)\n- `file` - read from `file`\n- `text` - the literal `text`\n- `none` - no body',
  'launchers.location': '- `menu` - a Start menu entry\n- `desktop` - a desktop shortcut',
  'services.start': '- `automatic` - starts at boot\n- `manual` - starts on demand\n- `disabled` - never starts',
  'prerequisite.target': '- `current` - the architecture being installed (default)\n- `x86`, `x64`, `arm64` - one specific architecture\n- `any` - any of them satisfies it',
  privilege: '- `user` - the signed-in user is enough\n- `system` - host-wide authority is required (default)',
  'requirement.kind': '- `runtime` - a runtime Zup recognises, e.g. `windows.webview2.evergreen`\n- `installed_package` - a package identifier the machine\'s package database uses\n- `file_version` - a literal absolute path and a version',
  'package.type': '- `embedded` - shipped inside the artifact, with a digest and a size\n- `remote` - fetched over HTTPS, with a digest and a file name',

  // `requirement` and `package` are `oneOf` sets of tagged variants, so there is
  // no single property map to generate. Each variant's fields are listed here.
  'prerequisite.requirement': [
    '| Key | Type | Required | Meaning |',
    '| --- | --- | --- | --- |',
    '| `kind` | `runtime` \\| `installed_package` \\| `file_version` | yes | Which kind of requirement this is |',
    '| `id` | `string` | for `runtime` and `installed_package` | What to look for. A lowercase dotted path, or a package identifier |',
    '| `path` | `string` | for `file_version` | A literal absolute path. No templates |',
    '| `version` | `string` | no | A semver requirement, e.g. `">=2.0"` |',
  ].join('\n'),
  'prerequisite.package': [
    '| Key | Type | Required | Meaning |',
    '| --- | --- | --- | --- |',
    '| `type` | `embedded` \\| `remote` | yes | Whether the package ships in the artifact or is fetched |',
    '| `path` | `string` | for `embedded` | A `/`-separated path relative to the project |',
    '| `url` | `string` | for `remote` | An HTTPS URL with no credentials and no fragment |',
    '| `filename` | `string` | for `remote` | A safe Windows file name |',
    '| `sha256` | `string` | yes | Lowercase hex SHA-256 of the package |',
    '| `size` | `integer` | for `embedded` | The byte size. Checked at build time |',
  ].join('\n'),
  privilege: '- `user` - the signed-in user is enough\n- `system` - host-wide authority is required (default)',
  'component.prominence.values': '- `auto` - the conservative default, described below\n- `primary` - the person should see this decision before installing\n- `secondary` - a sensible default; the group can stay out of the happy path',
  'component.selection.values': '- `defaulted` - the declared defaults are a valid choice (default)\n- `explicit` - at least one optional component in the group must be selected',

  // Prose pointers rather than tables.
  condition: 'See [conditions](/configure/files#conditions) for the grammar.',
  template: 'See [path templates](/configure/install#path-templates) for the vocabulary.',
  icon: 'See [icon](/configure/app#icon) for both authoring forms.',
}

let page = await readFile(pagePath, 'utf8')

for (const [name, body] of Object.entries(sections)) {
  const start = `<!-- generated:${name} -->`
  const end = '<!-- /generated -->'
  const from = page.indexOf(start)
  if (from === -1) throw new Error(`no marker for \`${name}\` in the page`)
  const stop = page.indexOf(end, from)
  if (stop === -1) throw new Error(`unterminated marker for \`${name}\` in the page`)
  page = page.slice(0, from + start.length) + '\n\n' + body + '\n\n' + page.slice(stop)
}

// A marker in the page with no entry above means a table has stopped
// regenerating without anybody noticing.
for (const match of page.matchAll(/<!-- generated:([^ ]+) -->/g)) {
  if (!(match[1] in sections)) {
    throw new Error(`marker \`${match[1]}\` in the page has no entry in \`sections\``)
  }
}

// The numbered `Targeted*` definitions are an artifact of the schema
// generator. Record the mapping for anyone reading the schema directly.
const mapping = Object.entries(targeted)
  .map(([def, key]) => `- \`${def}\` → \`${key}\``)
  .join('\n')
page = page.replace(/<!-- targeted -->[\s\S]*?<!-- \/targeted -->/, `<!-- targeted -->\n${mapping}\n<!-- /targeted -->`)

await writeFile(pagePath, page, 'utf8')
console.log(`wrote ${Object.keys(sections).length} tables to ${pagePath}`)
