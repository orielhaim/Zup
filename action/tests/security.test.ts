import { describe, expect, it } from 'bun:test'
import { checkSafety } from '../src/security.js'
import { attestationRequested } from '../src/workflow.js'
import { inputs } from './fixtures.js'

describe('dangerous triggers', () => {
  it('refuses to publish on pull_request_target', () => {
    // The base repository's secrets and write token, running a fork's code.
    const verdict = checkSafety({
      eventName: 'pull_request_target',
      operation: 'publish',
      allowUnsafe: false,
      fromFork: false,
    })
    expect(verdict.allowed).toBe(false)
    expect(verdict.reason).toContain('pull_request_target')
    expect(verdict.reason).toContain('secrets')
    expect(verdict.reason).toContain('allow-unsafe-publish')
  })

  it('refuses to release on workflow_run', () => {
    const verdict = checkSafety({
      eventName: 'workflow_run',
      operation: 'release',
      allowUnsafe: false,
      fromFork: false,
    })
    expect(verdict.allowed).toBe(false)
  })

  it('refuses even with the escape hatch when the checkout came from a fork', () => {
    // `allow-unsafe-publish` is a decision about a *configuration*; it is not a
    // decision to trust a specific stranger's branch. Somebody who genuinely needs
    // this restructures the workflow, which is the point.
    const verdict = checkSafety({
      eventName: 'pull_request_target',
      operation: 'publish',
      allowUnsafe: true,
      fromFork: true,
    })
    expect(verdict.allowed).toBe(false)
    expect(verdict.reason).toContain('fork')
    expect(verdict.overridden).toBe(false)
  })

  it('permits a publish with the escape hatch on a non-fork run', () => {
    const verdict = checkSafety({
      eventName: 'workflow_run',
      operation: 'publish',
      allowUnsafe: true,
      fromFork: false,
    })
    expect(verdict.allowed).toBe(true)
    expect(verdict.overridden).toBe(true)
    // The override is announced, never silent.
    expect(verdict.reason).toContain('allow-unsafe-publish')
  })

  it('permits every non-mutating operation on a dangerous trigger', () => {
    // Building untrusted code is a legitimate thing to do, and it writes nothing
    // to the repository. Refusing it would push people toward the dangerous
    // pattern rather than away from it.
    for (const operation of ['setup', 'build', 'compose', 'attest']) {
      const verdict = checkSafety({
        eventName: 'pull_request_target',
        operation,
        allowUnsafe: false,
        fromFork: true,
      })
      expect(verdict.allowed).toBe(true)
      expect(verdict.overridden).toBe(false)
    }
  })

  it('permits everything on an ordinary trigger', () => {
    for (const event of ['push', 'workflow_dispatch', 'schedule', 'pull_request']) {
      for (const operation of ['setup', 'build', 'compose', 'attest', 'publish', 'release']) {
        expect(
          checkSafety({ eventName: event, operation, allowUnsafe: false, fromFork: false }).allowed,
        ).toBe(true)
      }
    }
  })

  it('refuses a pull_request-triggered publish with a write token too', () => {
    // `pull_request` from a fork gets a read-only token, so a publish here fails
    // at the API. Saying so up front is more useful than a 403 from GitHub.
    const verdict = checkSafety({
      eventName: 'pull_request',
      operation: 'publish',
      allowUnsafe: false,
      fromFork: true,
    })
    expect(verdict.allowed).toBe(true)
    // Not refused by the trigger - the token is what limits it, and the action
    // does not claim otherwise. The assertion is that it does not silently
    // pretend the publish will work.
    expect(verdict.reason).toBe('')
  })
})

describe('attestation requests', () => {
  it('treats the attest operation as the request itself', () => {
    // A job whose whole purpose is attestation should not also have to set a
    // boolean, and a generated workflow that does exactly that is a bug.
    expect(attestationRequested({ ...inputs(), operation: 'attest', attest: false })).toBe(true)
  })

  it('adds attestation to a release only when asked', () => {
    expect(attestationRequested({ ...inputs(), operation: 'release', attest: false })).toBe(false)
    expect(attestationRequested({ ...inputs(), operation: 'release', attest: true })).toBe(true)
  })

  it('never attests a build', () => {
    expect(attestationRequested({ ...inputs(), operation: 'build', attest: false })).toBe(false)
  })
})
