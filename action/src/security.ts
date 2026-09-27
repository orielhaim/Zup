/**
 * Refusing to publish untrusted code with a privileged credential.
 *
 * `pull_request_target` and `workflow_run` run in the *base* repository's context,
 * with its secrets and its write token, while executing code that came from a
 * fork. A workflow that checks out the fork and then publishes has handed a
 * release credential to whoever opened the pull request. This is the most
 * exploited class of GitHub Actions vulnerability, and zup's publisher is an
 * unusually good target for it: a write token, the ability to create releases and
 * tags, and a build that runs first.
 *
 * `build`, `compose`, `setup` and `attest` are not refused. They do not write to
 * the repository, and building untrusted code on `pull_request_target` is a
 * legitimate thing to want.
 */

/** The events where base-repository privileges meet foreign code. */
const UNSAFE_EVENTS = new Set(['pull_request_target', 'workflow_run'])

/** The operations that write to the repository. */
const MUTATING = new Set(['publish', 'release'])

/** What the check decided. */
export interface SafetyVerdict {
  /** Whether the operation may proceed. */
  allowed: boolean
  /** Why, for the log and for a warning annotation. */
  reason: string
  /** Whether the developer opted in explicitly. */
  overridden: boolean
}

/** What the check needs to know. */
export interface SafetyContext {
  eventName: string
  operation: string
  allowUnsafe: boolean
  /**
   * Whether the checkout this job runs came from a fork.
   *
   * An input rather than something inferred here: from inside the job, a
   * `workflow_run` triggered by a fork's pull request looks identical to one
   * triggered by a branch push. When the two signals disagree, this errs toward
   * refusal.
   */
  fromFork: boolean
}

/** Decide whether a mutating operation may proceed. */
export function checkSafety(context: SafetyContext): SafetyVerdict {
  const unsafeEvent = UNSAFE_EVENTS.has(context.eventName)
  if (!unsafeEvent || !MUTATING.has(context.operation)) {
    return { allowed: true, reason: '', overridden: false }
  }
  const reason =
    `\`${context.operation}\` cannot run on \`${context.eventName}\`. ` +
    "That event gives this job the base repository's secrets and write token " +
    'while running code that may have come from a fork, so publishing here would ' +
    'hand a release credential to whoever wrote that code.'
  if (context.fromFork) {
    return {
      allowed: false,
      reason: `${reason} This run's checkout came from a fork.`,
      overridden: false,
    }
  }
  if (!context.allowUnsafe) {
    return {
      allowed: false,
      reason: `${reason} Set \`allow-unsafe-publish: true\` only if you have decided this is safe here.`,
      overridden: false,
    }
  }
  return {
    allowed: true,
    reason: `${reason} Continuing because \`allow-unsafe-publish: true\` was set.`,
    overridden: true,
  }
}

/**
 * Whether the token this job holds is absent.
 *
 * Deliberately not an inference about scopes. A job with `contents: read` and
 * `operation: publish` fails later inside the CLI with GitHub's own 403, which
 * names the missing permission more accurately than a guess here could.
 */
export function tokenMayBeInsufficient(context: { token: string | undefined }): boolean {
  return context.token === undefined
}
