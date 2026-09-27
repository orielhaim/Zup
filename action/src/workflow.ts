/**
 * The action's entry point: read inputs, resolve zup, run the phases, report.
 *
 * The environment of every subprocess is built from scratch, and the publish token
 * appears in exactly one of them. Not "is scrubbed from the others" — *absent* from
 * the others, because a scrub is a list of things somebody remembered to remove,
 * and a build that grows a new variable tomorrow is otherwise a credential leak.
 * That is why `buildEnvironment` takes the token as an explicit `undefined` for
 * every phase but the last: the type makes the omission deliberate.
 */

import { rm } from 'node:fs/promises'
import * as core from '@actions/core'
import * as toolCache from '@actions/tool-cache'
import {
  attestSubjects,
  digestOfFile,
  ManifestError,
  manifestArtifacts,
  parseManifest,
  shouldUploadDirect,
} from './artifacts.js'
import { InputError, type Inputs, readInputs } from './inputs.js'
import {
  argumentsFor,
  mergeResults,
  needsToken,
  type Phase,
  phasesFor,
  producesArtifacts,
  releaseManifestPath,
  withAdvanced,
} from './phases.js'
import { UnsupportedRunnerError } from './platform.js'
import type { Log } from './ports.js'
import { type OperationResult, parseResult, ResultFormatError } from './result.js'
import {
  assetName,
  checkoutIsFromFork,
  context,
  NodeFileSystem,
  SpawnRunner,
  ToolkitArtifactUploader,
  ToolkitAttestor,
  ToolkitDownloader,
  ToolkitLog,
  ZupReleaseSource,
} from './runtime.js'
import { checkSafety } from './security.js'
import { renderSummary, type Summary } from './summary.js'
import { type ResolvedTool, resolveTool, ToolError } from './tool.js'

/**
 * The zup version this action build was tested against.
 *
 * Not "latest from the internet": a workflow that pins an action ref should get
 * reproducible tool behaviour, and the way to have that is for the action to carry
 * the version it was built against. A developer who wants a different one says so
 * with `zup-version`. `latest` is still accepted, and the action warns in the log
 * because a non-reproducible install should not be silent.
 */
const TESTED_ZUP_VERSION = '0.0.1'

/** The GitHub repository zup's own releases are published under. */
const ZUP_REPOSITORY = 'orielhaim/zup'

/**
 * The variables a zup subprocess may see.
 *
 * An allowlist, so a credential GitHub adds to the environment next year is not in
 * a zup build by default. Kept small on purpose: the toolchain variables, the proxy
 * variables a corporate runner needs to reach the network, and the locale a build's
 * output formatting depends on. `CI` and the GitHub paths are here because zup
 * reports CI context in a build receipt.
 */
const INHERITED = [
  'CI',
  'GITHUB_ACTIONS',
  'GITHUB_WORKSPACE',
  'GITHUB_ACTION_PATH',
  'GITHUB_JOB',
  'GITHUB_RUN_ID',
  'GITHUB_RUN_ATTEMPT',
  'GITHUB_WORKFLOW',
  'GITHUB_REPOSITORY',
  'GITHUB_SERVER_URL',
  'GITHUB_API_URL',
  'GITHUB_OUTPUT',
  'GITHUB_ENV',
  'GITHUB_PATH',
  'GITHUB_STEP_SUMMARY',
  'RUNNER_OS',
  'RUNNER_ARCH',
  'RUNNER_TEMP',
  'RUNNER_TOOL_CACHE',
  'HOME',
  'LANG',
  'LC_ALL',
  'TMPDIR',
  'TEMP',
  'TMP',
  'APPDATA',
  'LOCALAPPDATA',
  'PROGRAMDATA',
  'USERPROFILE',
  'SystemRoot',
  'SystemDrive',
  'ComSpec',
  'PATHEXT',
  'NUMBER_OF_PROCESSORS',
  'PROCESSOR_ARCHITECTURE',
  'CARGO_HOME',
  'RUSTUP_HOME',
  'RUSTUP_TOOLCHAIN',
  'CARGO_TERM_COLOR',
  'RUST_BACKTRACE',
  'HTTPS_PROXY',
  'HTTP_PROXY',
  'NO_PROXY',
  'https_proxy',
  'http_proxy',
  'no_proxy',
] as const

/** The credential variable zup's publisher reads. */
const TOKEN_VARIABLES = ['GH_TOKEN', 'GITHUB_TOKEN'] as const

/** What the step did, so the summary and the outputs can report it. */
interface Outcome {
  performed: Phase[]
  result: OperationResult | undefined
  tool: ResolvedTool
  attestation: { requested: boolean; performed: boolean; subjects: number } | undefined
  failure: { message: string; remedy: string } | undefined
}

export async function run(): Promise<void> {
  const log = new ToolkitLog()
  const github = context()
  let outcome: Outcome | undefined

  try {
    const inputs = readInputs()

    // Registered before anything else can print it. A token that reaches the log
    // before `setSecret` was called is a token in the workflow log, which is
    // world-readable for a public repository and retained for a private one.
    if (inputs.token.value !== undefined && inputs.token.value.length > 0) {
      log.setSecret(inputs.token.value)
    }

    const verdict = checkSafety({
      eventName: github.eventName,
      operation: inputs.operation,
      allowUnsafe: inputs.allowUnsafePublish,
      fromFork: checkoutIsFromFork(github),
    })
    if (verdict.overridden) {
      log.warning(verdict.reason)
    }
    if (!verdict.allowed) {
      log.annotate('error', verdict.reason)
      log.fail(verdict.reason)
      return
    }

    const tool = await install(inputs, log, github)
    const result = await execute(inputs, tool, log, github)
    outcome = result

    report(inputs, result, log, github)
    if (result.failure !== undefined) {
      log.fail(`${result.failure.message} ${result.failure.remedy}`)
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    const remedy = remedyFor(error)
    log.annotate('error', message)
    log.fail(`${message} ${remedy}`)
    if (outcome === undefined) {
      log.debug('the step failed before any phase completed')
    }
  }
}

/** Install zup, or use the one the workflow pointed at. */
async function install(
  inputs: Inputs,
  log: ToolkitLog,
  github: ReturnType<typeof context>,
): Promise<ResolvedTool> {
  log.startGroup('Setup zup')
  try {
    const version = inputs.zupVersion ?? TESTED_ZUP_VERSION
    if (inputs.zupVersion === 'latest') {
      log.warning(
        '`zup-version: latest` resolves at run time, so this build is not reproducible. ' +
          'Pin an exact version unless you are deliberately testing the newest one.',
      )
    }
    const tool = await resolveTool(
      {
        zupPath: inputs.zupPath,
        zupVersion: inputs.zupVersion,
        // The runner comes from the workflow context rather than from `process.env`,
        // so the action's environment-reading surface is one file and this decision
        // is testable without a runner.
        runner: {
          RUNNER_OS: github.env['RUNNER_OS'],
          RUNNER_ARCH: github.env['RUNNER_ARCH'],
        },
      },
      {
        cache: {
          find: async (cacheVersion, arch) => toolCache.find(cacheVersion, arch),
          cacheFile: async (staged, fileName, cacheVersion, arch) => {
            const directory = await toolCache.cacheFile(staged, fileName, cacheVersion, arch)
            if (process.platform !== 'win32') {
              // `cacheFile` copies rather than moves — a move can fail on Windows
              // when antivirus holds a handle — so the staged copy is still there.
              // Removing it keeps a large download from lingering in `RUNNER_TEMP`.
              await rm(staged, { force: true }).catch(() => undefined)
            }
            return directory
          },
        },
        downloader: new ToolkitDownloader(),
        filesystem: new NodeFileSystem(),
        releases: new ZupReleaseSource(ZUP_REPOSITORY, github.apiUrl, version, log),
        log,
        releaseRepository: ZUP_REPOSITORY,
      },
      TESTED_ZUP_VERSION,
    )
    // `addPath` writes to GITHUB_PATH, which is how a step changes its *successors'*
    // environment without touching its own.
    core.addPath(dirname(tool.path))
    log.info(
      tool.source === 'explicit'
        ? `Using the zup at ${tool.path}`
        : `zup ${tool.version} ready (${tool.identity.platform}-${tool.identity.arch}, ${tool.source})`,
    )
    return tool
  } finally {
    log.endGroup()
  }
}

function dirname(target: string): string {
  const separator = target.includes('\\') && !target.includes('/') ? '\\' : '/'
  const index = target.lastIndexOf(separator)
  return index <= 0 ? target : target.slice(0, index)
}

/** Run every phase the operation expands to. */
async function execute(
  inputs: Inputs,
  tool: ResolvedTool,
  log: ToolkitLog,
  github: ReturnType<typeof context>,
): Promise<Outcome> {
  const filesystem = new NodeFileSystem()
  const runner = new SpawnRunner()
  const phases = phasesFor(inputs.operation)
  const results = new Map<Phase, OperationResult>()
  const performed: Phase[] = []
  let attestation: Outcome['attestation']

  for (const phase of phases) {
    log.startGroup(titleFor(phase))
    try {
      const result = await runPhase(phase, inputs, tool, log, github, runner, filesystem)
      if (result !== undefined) {
        results.set(phase, result)
        annotate(result, log, inputs.projectPath)
      }
      performed.push(phase)

      if (phase === 'attest') {
        attestation = await attest(inputs, log, filesystem)
      }
      if (producesArtifacts(phase) && inputs.uploadWorkflowArtifacts) {
        await upload(inputs, tool, log, filesystem, results.get(phase))
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error)
      log.error(message)
      log.endGroup()
      return {
        performed,
        result: mergeResults(phases, results),
        tool,
        attestation,
        failure: { message, remedy: remedyFor(error) },
      }
    }
    log.endGroup()
  }

  return {
    performed,
    result: mergeResults(phases, results),
    tool,
    attestation,
    failure: undefined,
  }
}

/** One phase: a zup invocation, or reading the manifest. */
async function runPhase(
  phase: Phase,
  inputs: Inputs,
  tool: ResolvedTool,
  log: ToolkitLog,
  github: ReturnType<typeof context>,
  runner: SpawnRunner,
  filesystem: NodeFileSystem,
): Promise<OperationResult | undefined> {
  if (phase === 'attest') {
    return attestResult(inputs, log, filesystem)
  }

  const args = withAdvanced(argumentsFor(phase, inputs), inputs)
  const environment = buildEnvironment(phase, inputs, github)
  log.debug(`zup ${args.join(' ')}`)

  // Streaming is right for a build: a developer watching a compile wants to see it
  // compile. The structured result still comes back on stdout, because
  // `getExecOutput` streams and captures the same run.
  const outcome = await runner.run({
    program: tool.path,
    args,
    cwd: inputs.projectPath,
    env: environment,
    stream: true,
  })

  if (outcome.code !== 0) {
    throw new CommandFailure(phase, outcome.code, outcome.stderr)
  }
  return parseResult(outcome.stdout, phase)
}

/** The environment a phase's subprocess sees. */
export function buildEnvironment(
  phase: Phase,
  inputs: Inputs,
  github: ReturnType<typeof context>,
): Record<string, string> {
  const environment: Record<string, string> = {}
  for (const name of INHERITED) {
    const value = github.env[name]
    if (value !== undefined && value.length > 0) {
      environment[name] = value
    }
  }
  // A dry run still needs to know it is one: `zup publish github` prints the plan
  // instead of creating a draft, and a workflow wants that visible.
  if (inputs.dryRun) {
    environment['ZUP_DRY_RUN'] = '1'
  }
  // The only line in this file that reads the token, guarded by `needsToken` rather
  // than by the operation string, so a new operation that publishes gets it by
  // construction rather than by remembering.
  if (needsToken(phase) && inputs.token.value !== undefined) {
    for (const name of TOKEN_VARIABLES) {
      environment[name] = inputs.token.value
    }
  }
  return environment
}

/** The result an `attest` phase produces: what the manifest says was built. */
async function attestResult(
  inputs: Inputs,
  log: ToolkitLog,
  filesystem: NodeFileSystem,
): Promise<OperationResult> {
  const path = filesystem.resolve(inputs.projectPath, releaseManifestPath(inputs.releaseDir))
  if (!(await filesystem.exists(path))) {
    throw new MissingManifest(path, inputs.releaseDir)
  }
  const manifest = parseManifest(await filesystem.readText(path), path)
  const artifacts = manifestArtifacts(manifest)
  log.info(
    `${artifacts.length} artifact${artifacts.length === 1 ? '' : 's'} in the release manifest`,
  )
  return {
    schema: 1,
    operation: 'attest',
    success: true,
    appVersion: manifest.application.version,
    targets: manifest.variants.map((variant) => variant.target),
    artifacts,
    releaseManifest: releaseManifestPath(inputs.releaseDir),
    diagnostics: [],
  }
}

/**
 * Whether this run should create attestations.
 *
 * `operation: attest` *is* the request — a job whose whole purpose is attestation
 * should not also have to set a boolean. `attest: true` adds the phase to
 * `operation: release`, where the other phases are a build and a publication and
 * attestation is one more thing to opt into.
 */
export function attestationRequested(inputs: Inputs): boolean {
  return inputs.attest || inputs.operation === 'attest'
}

/** Attest the final bytes, discovered from the release manifest. */
async function attest(
  inputs: Inputs,
  log: ToolkitLog,
  filesystem: NodeFileSystem,
): Promise<{ requested: boolean; performed: boolean; subjects: number }> {
  if (!attestationRequested(inputs)) {
    return { requested: false, performed: false, subjects: 0 }
  }
  if (inputs.dryRun) {
    log.info('skipping attestation: this is a dry run')
    return { requested: true, performed: false, subjects: 0 }
  }
  const manifestAbsolute = filesystem.resolve(
    inputs.projectPath,
    releaseManifestPath(inputs.releaseDir),
  )
  if (!(await filesystem.exists(manifestAbsolute))) {
    throw new MissingManifest(manifestAbsolute, inputs.releaseDir)
  }
  const manifest = parseManifest(await filesystem.readText(manifestAbsolute), manifestAbsolute)
  const subjects = await attestSubjects(
    manifestAbsolute,
    manifest,
    inputs.releaseDir,
    inputs.attestPaths,
    {
      resolve: (...segments) => filesystem.resolve(inputs.projectPath, ...segments),
      exists: (candidate) => filesystem.exists(candidate),
      digest: digestOfFile,
    },
  )
  log.info(`attesting ${subjects.length} subject${subjects.length === 1 ? '' : 's'}`)
  await new ToolkitAttestor().attest(subjects)
  return { requested: true, performed: true, subjects: subjects.length }
}

/** Upload the release directory, directly when it is a single compressed file. */
async function upload(
  inputs: Inputs,
  tool: ResolvedTool,
  log: ToolkitLog,
  filesystem: NodeFileSystem,
  result: OperationResult | undefined,
): Promise<void> {
  const directory = filesystem.resolve(inputs.projectPath, inputs.releaseDir)
  if (!(await filesystem.exists(directory))) {
    throw new MissingOutput(directory)
  }
  const name =
    inputs.workflowArtifactName ?? `${tool.version}-${tool.identity.platform}-${tool.identity.arch}`
  const direct = shouldUploadDirect([directory])
  log.info(
    direct
      ? `uploading ${directory} as an unarchived artifact`
      : `uploading ${directory} as an archived artifact`,
  )
  const uploaded = await new ToolkitArtifactUploader().upload({
    name,
    paths: [directory],
    rootDirectory: directory,
    retentionDays: inputs.artifactRetentionDays,
    direct,
  })
  if (uploaded.name !== name) {
    log.notice(
      `the service named the artifact \`${uploaded.name}\` rather than \`${name}\`, ` +
        'because an unarchived artifact takes the file name. Reference it as written.',
    )
  }
  if (result !== undefined) {
    for (const artifact of result.artifacts) {
      log.debug(`${artifact.path} ${artifact.digest}`)
    }
  }
}

/** Turn a result's diagnostics into annotations. */
export function annotate(result: OperationResult, log: Log, projectPath: string): void {
  for (const diagnostic of result.diagnostics) {
    const location =
      diagnostic.source === undefined
        ? undefined
        : {
            file: absolute(diagnostic.source.file, projectPath),
            startLine: diagnostic.source.startLine,
            startColumn: diagnostic.source.startColumn,
            endLine: diagnostic.source.endLine,
            endColumn: diagnostic.source.endColumn,
          }
    const message =
      diagnostic.help === undefined
        ? diagnostic.message
        : `${diagnostic.message}\n${diagnostic.help}`
    log.annotate(diagnostic.severity, `${diagnostic.code}: ${message}`, location)
  }
}

function absolute(file: string, projectPath: string): string {
  // GitHub resolves a relative annotation path against the workspace root, and a
  // zup diagnostic is relative to the project. A project in a subdirectory is the
  // common case, and an annotation on a path that does not resolve is invisible.
  if (file.startsWith('/') || /^[A-Za-z]:[\\/]/u.test(file)) {
    return file
  }
  return `${projectPath.replace(/[\\/]+$/u, '')}/${file}`
}

/** Declare the outputs and write the summary. */
function report(
  inputs: Inputs,
  outcome: Outcome,
  log: ToolkitLog,
  _github: ReturnType<typeof context>,
): void {
  const result = outcome.result
  log.setOutput('zup-path', outcome.tool.path)
  log.setOutput('zup-version', outcome.tool.version)
  if (result?.appVersion !== undefined) {
    log.setOutput('app-version', result.appVersion)
  }
  if (result?.artifacts.length) {
    // JSON, not a newline-joined list: a caller that needs to iterate has to parse,
    // and JSON is the one form that survives a path with a space in it.
    log.setOutput(
      'artifact-paths',
      JSON.stringify(
        result.artifacts.map((artifact) => ({
          path: artifact.path,
          size: artifact.size,
          digest: artifact.digest,
        })),
      ),
    )
  }
  if (result?.releaseManifest !== undefined) {
    log.setOutput('release-manifest', result.releaseManifest)
  }
  if (result?.release?.releaseId !== undefined && result.release.releaseId > 0) {
    log.setOutput('release-id', String(result.release.releaseId))
  }
  if (result?.release?.url !== undefined) {
    log.setOutput('release-url', result.release.url)
  }
  if (inputs.receipt !== undefined && result?.release !== undefined) {
    log.setOutput('publish-receipt', inputs.receipt)
  }

  const summary: Summary = {
    operation: inputs.operation,
    tool: outcome.tool,
    performed: outcome.performed,
    result,
    failure: outcome.failure,
    dryRun: inputs.dryRun,
  }
  log.summary(renderSummary(summary))
}

function titleFor(phase: Phase): string {
  switch (phase) {
    case 'build':
      return 'Build'
    case 'compose':
      return 'Compose'
    case 'attest':
      return 'Attest'
    case 'publish':
      return 'Publish'
  }
}

/** A zup command that exited non-zero. */
export class CommandFailure extends Error {
  constructor(
    readonly phase: Phase,
    readonly code: number,
    readonly stderr: string,
  ) {
    const detail = stderr.trim().split('\n').slice(-3).join(' ').slice(0, 400)
    super(`zup ${phase} exited with code ${code}${detail.length > 0 ? `: ${detail}` : ''}`)
    this.name = 'CommandFailure'
  }
}

/** The release directory a phase needed is not there. */
export class MissingOutput extends Error {
  constructor(readonly path: string) {
    super(
      `${path} does not exist. ` +
        'A phase that uploads or attests needs the build before it; check the ' +
        '`needs:` between your jobs, and that `release-dir` names the same ' +
        'directory in each of them.',
    )
    this.name = 'MissingOutput'
  }
}

/** The release manifest a phase needed is not there. */
export class MissingManifest extends Error {
  constructor(
    readonly path: string,
    readonly releaseDir: string,
  ) {
    super(
      `no release manifest at ${path}. ` +
        `\`zup build --release-manifest ${releaseDir}/zup-release.json\` writes one; ` +
        'without it there is nothing to attest and no release to publish.',
    )
    this.name = 'MissingManifest'
  }
}

/**
 * What to do about a failure, on top of what went wrong.
 *
 * Empty when there is nothing useful to add, because the caller concatenates it. An
 * earlier version returned `error.message` for a generic error, so a failure the
 * action had no advice for printed its own message twice.
 */
export function remedyFor(error: unknown): string {
  if (error instanceof InputError) {
    return error.remedy
  }
  if (error instanceof ResultFormatError) {
    return error.message
  }
  if (error instanceof CommandFailure) {
    return 'Read the output above: zup printed why. Rerun with `ACTIONS_STEP_DEBUG: true` for the exact command line.'
  }
  if (
    error instanceof MissingOutput ||
    error instanceof MissingManifest ||
    error instanceof UnsupportedRunnerError ||
    error instanceof ToolError
  ) {
    // These carry their own remedy in the message.
    return ''
  }
  if (error instanceof ManifestError) {
    return error.remedy
  }
  if (error instanceof Error) {
    // Unrecognised: its message is the whole report, and inventing advice for it
    // would be worse than saying nothing.
    return ''
  }
  return 'See the log above.'
}

export { assetName, INHERITED, TESTED_ZUP_VERSION, TOKEN_VARIABLES, ZUP_REPOSITORY }
