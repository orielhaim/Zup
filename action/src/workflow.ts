/**
 * The action's entry point: read inputs, resolve zup, run the phases, report.
 *
 * The environment of every subprocess is built from scratch, and the publish token
 * appears in exactly one of them. Not "is scrubbed from the others" - *absent* from
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
  parseManifest,
  type ReleaseManifestDocument,
  shouldUploadDirect,
} from './artifacts.js'
import { InputError, type Inputs, readInputs } from './inputs.js'
import {
  argumentsFor,
  needsToken,
  operationFor,
  type Phase,
  phasesFor,
  producesArtifacts,
  releaseManifestPath,
} from './phases.js'
import { UnsupportedRunnerError } from './platform.js'
import type { GithubContext, Log } from './ports.js'
import {
  type AutomationResult,
  conformance,
  coversBytes,
  type Diagnostic,
  ProtocolError,
  parseEvent,
} from './protocol.js'
import {
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
import { LineFramer } from './stream.js'
import { mergeResults, renderSummary, type Summary } from './summary.js'
import { type ResolvedTool, resolveTool, ToolError, toolDirectory } from './tool.js'

/**
 * The zup version this action build was tested against.
 *
 * Not "latest from the internet": a workflow that pins an action ref should get
 * reproducible tool behaviour, and the way to have that is for the action to carry
 * the version it was built against. A developer who wants a different one says so
 * with `zup-version`.
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
 * output formatting depends on.
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
  results: Map<Phase, AutomationResult>
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
    // world-readable for a public repository.
    if (inputs.token !== undefined && inputs.token.length > 0) {
      log.setSecret(inputs.token)
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

    report(inputs, result, log)
    if (result.failure !== undefined) {
      log.fail(`${result.failure.message} ${result.failure.remedy}`)
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    log.annotate('error', message)
    log.fail(`${message} ${remedyFor(error)}`)
    if (outcome === undefined) {
      log.debug('the step failed before any phase completed')
    }
  }
}

/** Install zup, or use the one the workflow pointed at. */
async function install(inputs: Inputs, log: Log, github: GithubContext): Promise<ResolvedTool> {
  log.startGroup('Setup zup')
  try {
    const version = inputs.zupVersion ?? TESTED_ZUP_VERSION
    if (inputs.zupVersion === 'latest') {
      log.warning(
        '`zup-version: latest` resolves at run time, so this build is not reproducible. ' +
          'Pin an exact version unless you are deliberately testing the newest one.',
      )
    }
    const filesystem = new NodeFileSystem()
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
              // `cacheFile` copies rather than moves - a move can fail on Windows
              // when antivirus holds a handle - so the staged copy is still there.
              // Removing it keeps a large download from lingering in `RUNNER_TEMP`.
              await rm(staged, { force: true }).catch(() => undefined)
            }
            return directory
          },
        },
        downloader: new ToolkitDownloader(),
        filesystem,
        releases: new ZupReleaseSource(ZUP_REPOSITORY, github.apiUrl, version, log),
        log,
        releaseRepository: ZUP_REPOSITORY,
      },
      TESTED_ZUP_VERSION,
    )
    // `addPath` writes to GITHUB_PATH, which is how a step changes its *successors'*
    // environment without touching its own.
    core.addPath(toolDirectory(tool, filesystem))
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

/** Run every phase the operation expands to. */
async function execute(
  inputs: Inputs,
  tool: ResolvedTool,
  log: Log,
  github: GithubContext,
): Promise<Outcome> {
  const filesystem = new NodeFileSystem()
  const runner = new SpawnRunner()
  const phases = phasesFor(inputs.operation)
  const results = new Map<Phase, AutomationResult>()
  const annotator = new Annotator(log, inputs.projectPath)
  const performed: Phase[] = []
  let attestation: Outcome['attestation']

  for (const phase of phases) {
    log.startGroup(titleFor(phase))
    try {
      await runPhase(phase, inputs, tool, log, github, runner, filesystem, results, annotator)
      performed.push(phase)

      if (phase === 'attest') {
        attestation = await attest(inputs, log, filesystem)
      }
      if (producesArtifacts(phase) && inputs.uploadWorkflowArtifacts) {
        await upload(inputs, tool, log, github, filesystem, results.get(phase))
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error)
      log.error(message)
      log.endGroup()
      return {
        performed,
        results,
        tool,
        attestation,
        failure: { message, remedy: remedyFor(error) },
      }
    }
    log.endGroup()
  }

  return { performed, results, tool, attestation, failure: undefined }
}

/**
 * One phase.
 *
 * Two shapes, and the difference is which process answers the question. A zup
 * phase reads the protocol stream the command wrote. `attest` runs no zup command
 * at all - zup does not talk to Sigstore - so it reads the release description
 * itself, which is the document that says which bytes a downloader will receive.
 */
async function runPhase(
  phase: Phase,
  inputs: Inputs,
  tool: ResolvedTool,
  log: Log,
  github: GithubContext,
  runner: SpawnRunner,
  filesystem: NodeFileSystem,
  results: Map<Phase, AutomationResult>,
  annotator: Annotator,
): Promise<void> {
  if (phase === 'attest') {
    const { relative, document } = await readReleaseManifest(inputs, filesystem)
    const unsigned = document.artifacts.filter(
      (artifact) => !coversBytes(artifact.finalized?.evidence),
    )
    log.info(
      `${document.artifacts.length} artifact${document.artifacts.length === 1 ? '' : 's'} in ` +
        `\`${relative}\`` +
        (unsigned.length > 0
          ? `, ${unsigned.length} with no signature: ${unsigned.map((a) => a.path).join(', ')}`
          : ''),
    )
    return
  }
  const result = await runZup(phase, inputs, tool, log, github, runner, annotator)
  results.set(phase, result)
  annotate(result, log, inputs.projectPath, annotator)
}

/**
 * Run one phase's zup command and read the stream it wrote.
 *
 * The exit code and the document are read together, which is the only way they can
 * be made to agree: a nonzero exit is a failure *with* its diagnostics rather than a
 * stderr scrape, so a machine-format run that failed still says which check failed
 * and where.
 */
async function runZup(
  phase: Phase,
  inputs: Inputs,
  tool: ResolvedTool,
  log: Log,
  github: GithubContext,
  runner: SpawnRunner,
  annotator: Annotator,
): Promise<AutomationResult> {
  const operation = operationFor(phase)
  const args = argumentsFor(phase, inputs)
  log.debug(`zup ${args.join(' ')}`)

  // A framer rather than a string: a build reports a failing check in the first
  // second, and a step that only speaks at the end cannot be stopped.
  let result: AutomationResult | undefined
  const framer = new LineFramer((line) => {
    const event = parseEvent(line)
    if (event === undefined) {
      return
    }
    if (event.type === 'completed') {
      result = event.result
      return
    }
    reportEvent(event, log, annotator)
  })

  const outcome = await runner.run({
    program: tool.path,
    args,
    cwd: inputs.projectPath,
    env: buildEnvironment(phase, inputs, github),
    stream: true,
    stdout: framer,
  })

  if (result === undefined) {
    // No `completed` line. A run that failed before it could write one is a zup
    // bug or a zup that is too old for this action; both are worth saying plainly
    // rather than reporting an empty success.
    throw new ProtocolError(
      `the stream ended without a result (exit code ${outcome.code})`,
      outcome.stdout,
    )
  }
  const problem = conformance(result)
  if (problem !== undefined) {
    throw new ProtocolError(problem, outcome.stdout)
  }
  if (outcome.code !== 0 && result.status !== 'failure') {
    // zup exited nonzero and said it succeeded. The document and the exit code are
    // one answer, and this is the state where they are not.
    throw new ProtocolError(
      `\`${result.operation}\` exited with code ${outcome.code} but reported success`,
      outcome.stdout,
    )
  }
  if (operation !== undefined && result.operation !== operation) {
    throw new ProtocolError(
      `expected a \`${operation}\` result and got \`${result.operation}\``,
      outcome.stdout,
    )
  }
  if (outcome.code !== 0) {
    // A failure zup described. The document says which check failed and where; the
    // exit code only says that it did. Reporting the document is the whole reason
    // `--format jsonl` writes a result even when the run failed.
    throw new ReportedFailure(result, outcome.code)
  }
  return result
}

/**
 * A zup command that failed and said why.
 *
 * Carries the result rather than a formatted message, so the caller can annotate
 * the diagnostics and the summary can show the artifacts that were produced before
 * the failure rather than nothing at all.
 */
export class ReportedFailure extends Error {
  constructor(
    readonly result: AutomationResult,
    readonly code: number,
  ) {
    const reasons = result.diagnostics
      .map((diagnostic) => `${diagnostic.code}: ${diagnostic.message}`)
      .join('; ')
    super(
      `zup ${result.operation} failed (exit code ${code})${
        reasons.length > 0 ? `: ${reasons}` : ''
      }`,
    )
    this.name = 'ReportedFailure'
  }
}

/** What one streamed line tells the reader, and nothing more. */
function reportEvent(
  event: NonNullable<ReturnType<typeof parseEvent>>,
  log: Log,
  annotator: Annotator,
): void {
  switch (event.type) {
    case 'phase':
    case 'log':
      log.info(event.message)
      return
    case 'progress':
      log.info(`${event.label} ${event.completed}/${event.total}`)
      return
    case 'diagnostic':
      annotator.one(event.diagnostic)
      return
    case 'artifact':
      log.info(`${event.artifact.path} · ${event.artifact.digest.value.slice(0, 16)}…`)
      return
    case 'publication':
      log.info(
        `${event.publication.provider} ${event.publication.subject} ${event.publication.tag} ` +
          `(${event.publication.state})`,
      )
      return
    case 'version':
      log.debug(`zup ${event.zup} speaks protocol ${event.protocol}`)
      return
    case 'completed':
      return
  }
}

/**
 * The environment a phase's subprocess sees.
 *
 * The token is absent rather than scrubbed: a scrub is a list of things somebody
 * remembered to remove, and a build that grows a new variable tomorrow is otherwise
 * a credential leak. `needsToken` rather than the operation string, so a new
 * operation that publishes gets the token by construction rather than by
 * remembering.
 */
export function buildEnvironment(
  phase: Phase,
  inputs: Inputs,
  github: GithubContext,
): Record<string, string> {
  const environment: Record<string, string> = {}
  for (const name of INHERITED) {
    const value = github.env[name]
    if (value !== undefined && value.length > 0) {
      environment[name] = value
    }
  }
  // A dry run is a flag on the command, not an environment variable. There was a
  // `ZUP_DRY_RUN` here that nothing in zup read: the action believed it was asking
  // for something and zup was not being asked, so a dry-run release published
  // anyway. One contract, on the command line, where the parser can refuse it.
  if (inputs.token !== undefined && needsToken(phase)) {
    for (const name of TOKEN_VARIABLES) {
      environment[name] = inputs.token
    }
  }
  return environment
}

/** The release manifest, resolved, checked for existence, and parsed. */
async function readReleaseManifest(
  inputs: Inputs,
  filesystem: NodeFileSystem,
): Promise<{ absolute: string; relative: string; document: ReleaseManifestDocument }> {
  const relative = releaseManifestPath(inputs.releaseDir)
  const absolute = filesystem.resolve(inputs.projectPath, relative)
  if (!(await filesystem.exists(absolute))) {
    throw new MissingManifest(absolute, inputs.releaseDir)
  }
  return {
    absolute,
    relative,
    document: parseManifest(await filesystem.readText(absolute), absolute),
  }
}

/**
 * Whether this run should create attestations.
 *
 * `operation: attest` *is* the request - a job whose whole purpose is attestation
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
  log: Log,
  filesystem: NodeFileSystem,
): Promise<{ requested: boolean; performed: boolean; subjects: number }> {
  if (!attestationRequested(inputs)) {
    return { requested: false, performed: false, subjects: 0 }
  }
  if (inputs.dryRun) {
    log.info('skipping attestation: this is a dry run')
    return { requested: true, performed: false, subjects: 0 }
  }
  const { absolute, document } = await readReleaseManifest(inputs, filesystem)
  const subjects = await attestSubjects(absolute, document, inputs.releaseDir, inputs.attestPaths, {
    resolve: (...segments) => filesystem.resolve(inputs.projectPath, ...segments),
    exists: (candidate) => filesystem.exists(candidate),
    digest: digestOfFile,
  })
  log.info(`attesting ${subjects.length} subject${subjects.length === 1 ? '' : 's'}`)
  await new ToolkitAttestor().attest(subjects)
  return { requested: true, performed: true, subjects: subjects.length }
}

/** Upload the release directory, directly when it is a single compressed file. */
async function upload(
  inputs: Inputs,
  tool: ResolvedTool,
  log: Log,
  github: GithubContext,
  filesystem: NodeFileSystem,
  result: AutomationResult | undefined,
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
  const uploaded = await new ToolkitArtifactUploader(github).upload({
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
  for (const artifact of result?.artifacts ?? []) {
    log.debug(`${artifact.path} sha256:${artifact.digest.value}`)
  }
}

/**
 * Turning diagnostics into annotations, once each.
 *
 * A diagnostic that arrives mid-stream is annotated as it arrives, and the same
 * diagnostic arrives again in the final result. Telling the reader the same fact
 * twice is worse than not telling them at all, so the annotator remembers what it
 * has already said. Per run rather than per module: a second step in the same
 * process is a different run against a different log.
 */
export class Annotator {
  readonly #seen = new Set<string>()

  constructor(
    private readonly log: Log,
    private readonly projectPath: string,
  ) {}

  one(diagnostic: Diagnostic): void {
    const key = this.#identity(diagnostic)
    if (this.#seen.has(key)) {
      return
    }
    this.#seen.add(key)
    const source = diagnostic.source
    const location =
      source === null
        ? undefined
        : {
            file: absolute(source.file, this.projectPath),
            startLine: source.start_line ?? undefined,
            startColumn: source.start_column ?? undefined,
            endLine: source.end_line ?? undefined,
            endColumn: source.end_column ?? undefined,
          }
    const message =
      diagnostic.help === null ? diagnostic.message : `${diagnostic.message}\n${diagnostic.help}`
    this.log.annotate(diagnostic.severity, `${diagnostic.code}: ${message}`, location)
  }

  all(diagnostics: readonly Diagnostic[]): void {
    for (const diagnostic of diagnostics) {
      this.one(diagnostic)
    }
  }

  /// The same message about two different lines is two problems, so the location
  /// is part of what makes a diagnostic the same diagnostic.
  #identity(diagnostic: Diagnostic): string {
    const source = diagnostic.source
    return [diagnostic.code, diagnostic.message, source?.file ?? '', source?.start_line ?? ''].join(
      ' ',
    )
  }
}

/**
 * Turn a result's diagnostics into annotations.
 *
 * The annotator is passed rather than created so a run annotates each diagnostic
 * once across the stream and the final result; a caller with no state to keep gets
 * a fresh one, which is the same behaviour with less to say.
 */
export function annotate(
  result: AutomationResult,
  log: Log,
  projectPath: string,
  annotator: Annotator = new Annotator(log, projectPath),
): void {
  annotator.all(result.diagnostics)
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
function report(inputs: Inputs, outcome: Outcome, log: Log): void {
  const result = mergeResults(outcome.performed, outcome.results)
  log.setOutput('zup-path', outcome.tool.path)
  log.setOutput('zup-version', outcome.tool.version)
  if (result?.application !== null && result?.application !== undefined) {
    log.setOutput('app-version', result.application.version)
  }
  if (result?.artifacts.length) {
    // JSON, not a newline-joined list: JSON is the one form that survives a path
    // with a space in it.
    log.setOutput(
      'artifact-paths',
      JSON.stringify(
        result.artifacts.map((artifact) => ({
          path: artifact.path,
          size: artifact.size,
          digest: artifact.digest.value,
        })),
      ),
    )
  }
  const manifest = result?.release_manifest ?? undefined
  if (manifest !== undefined) {
    log.setOutput('release-manifest', manifest)
  }
  const publication = result?.publication
  if (publication !== null && publication !== undefined) {
    if (publication.id !== null && publication.id.length > 0) {
      log.setOutput('release-id', publication.id)
    }
    if (publication.url !== null) {
      log.setOutput('release-url', publication.url)
    }
  }
  if (inputs.receipt !== undefined && publication !== null && publication !== undefined) {
    log.setOutput('publish-receipt', inputs.receipt)
  }

  const summary: Summary = {
    operation: inputs.operation,
    tool: outcome.tool,
    performed: outcome.performed,
    results: outcome.results,
    result,
    failure: outcome.failure,
    dryRun: inputs.dryRun,
  }
  log.summary(renderSummary(summary))
}

function titleFor(phase: Phase): string {
  switch (phase) {
    case 'check':
      return 'Check'
    case 'build':
      return 'Build'
    case 'compose':
      return 'Compose'
    case 'finalize':
      return 'Finalize'
    case 'attest':
      return 'Attest'
    case 'publish':
      return 'Publish'
  }
}

/** The release directory a phase needed is not there. */
export class MissingOutput extends Error {
  constructor(readonly path: string) {
    super(
      `${path} does not exist. A phase that uploads or attests needs the build before it; ` +
        'check the `needs:` between your jobs, and that `release-dir` names the same ' +
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
        `\`zup build --release-manifest ${releaseDir}/zup-release.json\` writes one.`,
    )
    this.name = 'MissingManifest'
  }
}

/**
 * What to do about a failure, on top of what went wrong.
 *
 * Empty when there is nothing useful to add, because the caller concatenates it.
 * Errors whose message already carries their own advice return `''`, so a failure
 * the action had no advice for does not print the same sentence twice.
 */
export function remedyFor(error: unknown): string {
  if (error instanceof InputError || error instanceof ManifestError) {
    return error.remedy
  }
  if (error instanceof ReportedFailure) {
    return 'Every diagnostic above is annotated with its code and location. Rerun with `ACTIONS_STEP_DEBUG: true` for the exact command line.'
  }
  if (
    error instanceof ProtocolError ||
    error instanceof MissingOutput ||
    error instanceof MissingManifest ||
    error instanceof UnsupportedRunnerError ||
    error instanceof ToolError
  ) {
    return ''
  }
  return 'See the log above.'
}

export { INHERITED, TESTED_ZUP_VERSION, TOKEN_VARIABLES, ZUP_REPOSITORY }
