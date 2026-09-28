import { describe, expect, it } from 'bun:test'
import {
  accepts,
  conformance,
  coversBytes,
  isSigned,
  PROTOCOL_MAJOR,
  ProtocolError,
  parseEvent,
  parseResult,
} from '../src/protocol.js'
import { LineFramer } from '../src/stream.js'
import { document, fixture, fixtureNames, streamLines } from './protocol-fixtures.js'

describe('the fixtures zup produces', () => {
  it('are all documents this action can read', () => {
    // The gate: every golden fixture parses, conforms, and passes the version
    // check. A change to the Rust DTOs that the action cannot read fails here
    // rather than in a workflow.
    for (const name of fixtureNames().filter((entry) => entry !== 'jsonl-progress')) {
      const result = parseResult(fixture(name))
      expect(result.protocol).toBe('1.0')
      expect(result.operation.length).toBeGreaterThan(0)
      expect(conformance(result)).toBeUndefined()
    }
  })

  it('include a document for every operation that has its own payload', () => {
    // One fixture per `Details` variant. A variant with no fixture is a variant no
    // consumer has ever read, and the generated TypeScript is the only place its
    // shape exists.
    const kinds = new Set(
      fixtureNames()
        .filter((name) => name !== 'jsonl-progress')
        .map((name) => (document(name)['details'] as { kind?: string } | null)?.kind),
    )
    for (const kind of [
      'build',
      'check',
      'plan',
      'doctor',
      'publish.stage',
      'publish.github',
      'sign.prepare',
      'sign.verify',
      'artifact.inspect',
      'toolchain.status',
    ]) {
      expect([...kinds]).toContain(kind)
    }
  })
})

describe('parseResult', () => {
  it('reads the build zup actually wrote', () => {
    const result = parseResult(fixture('build-success'), 'build')
    expect(result.status).toBe('success')
    expect(result.application?.version).toBe('1.4.0')
    expect(result.application?.name).toBe('Acme')
    expect(result.release_manifest).toBe('dist/zup-release.json')
    const artifact = result.artifacts[0]
    expect(artifact?.path).toBe('Acme-Windows-Setup.exe')
    expect(artifact?.digest.algorithm).toBe('sha256')
    expect(artifact?.digest.value).toMatch(/^[0-9a-f]{64}$/u)
    expect(artifact?.size).toBe(248_512_896)
    expect(artifact?.kind).toBe('single')
    expect(artifact?.id).toBe('windows-x64')
    // `null` and `[]` are different facts: nobody looked, or looked and found
    // nothing. A consumer that conflates them reports an unverified release as an
    // unsigned one.
    expect(artifact?.signing).toBeNull()
    expect(result.summary).toBe('Built 1 artifact for 1 target')
  })

  it('reads a failure with a diagnostic and the file it is about', () => {
    const result = parseResult(fixture('build-failure'), 'build')
    expect(result.status).toBe('failure')
    expect(result.artifacts).toEqual([])
    const diagnostic = result.diagnostics[0]
    expect(diagnostic?.severity).toBe('error')
    expect(diagnostic?.code).toBe('zup.manifest.unknown_target')
    expect(diagnostic?.source?.file).toBe('zup.toml')
    expect(diagnostic?.source?.start_line).toBeNull()
    expect(diagnostic?.help).toContain('[build.targets.linux]')
  })

  it('reads a check that found something without failing the project', () => {
    // A warning on a success is the case the severity/status agreement exists for:
    // the project is valid, and a consumer that failed on this would be wrong.
    const result = parseResult(fixture('check-not-composable'), 'check')
    expect(result.status).toBe('success')
    expect(result.diagnostics[0]?.severity).toBe('warning')
    expect(result.diagnostics[0]?.code).toBe('zup.check.not_composable')
    expect(result.details?.kind).toBe('check')
  })

  it('reads a plan, which is a prediction rather than a change', () => {
    const result = parseResult(fixture('plan'), 'plan')
    expect(result.status).toBe('success')
    expect(result.artifacts).toEqual([])
    expect(result.details?.kind).toBe('plan')
  })

  it('reads the whole doctor check table, not only the failures', () => {
    const result = parseResult(fixture('doctor'), 'doctor')
    expect(result.status).toBe('failure')
    if (result.details?.kind !== 'doctor') {
      expect.unreachable('the payload is a doctor report')
    }
    expect(result.details.targets[0]?.status).toBe('fail')
    // A skipped check is a question that was never answered, which is a different
    // fact from a passed one and is the reason the table crosses at all.
    const skipped = result.details.targets[0]?.checks.find((check) => check.kind === 'update_root')
    expect(skipped?.status).toBe('skip')
    expect(skipped?.path).toBe('zup.toml')
  })

  it('reads a staged tree with its packages', () => {
    const result = parseResult(fixture('publish-stage'), 'publish.stage')
    expect(result.status).toBe('success')
    if (result.details?.kind !== 'publish.stage') {
      expect.unreachable('the payload is a staged tree')
    }
    expect(result.details.channel).toBe('stable')
    expect(result.details.packages[0]?.names).toEqual(['Acme-Windows-x64.zup'])
    expect(result.details.tuf_inputs).toBe(2)
  })

  it('reads a publication with its state, tag and assets', () => {
    const result = parseResult(fixture('publish-success'), 'publish.github')
    const publication = result.publication
    expect(publication).not.toBeNull()
    expect(publication?.provider).toBe('github')
    expect(publication?.subject).toBe('acme/acme')
    expect(publication?.tag).toBe('v1.4.0')
    expect(publication?.id).toBe('1234567890123456789')
    expect(publication?.state).toBe('published')
    expect(publication?.immutable).toBe(true)
    expect(publication?.url).toContain('/releases/tag/')
    expect(publication?.assets[0]?.digest?.value).toMatch(/^[0-9a-f]{64}$/u)
    expect(publication?.assets[0]?.state).toBe('uploaded')
  })

  it('reads a refused publication as a failure with the step that refused', () => {
    // The exit code and the document agree here, and the document says which asset
    // and which provider step - which a stderr scrape could not.
    const result = parseResult(fixture('publish-conflict'), 'publish.github')
    expect(result.status).toBe('failure')
    expect(result.diagnostics[0]?.code).toBe('zup.publish.asset_conflict')
    if (result.details?.kind !== 'publish.github') {
      expect.unreachable('the payload is a publication')
    }
    expect(result.details.failures[0]?.phase).toBe('Uploading')
    expect(result.details.failures[0]?.step).toBe('Acme-Windows-Setup.exe')
  })

  it('reads the signing list a project has to work through', () => {
    const result = parseResult(fixture('sign-prepare'), 'sign.prepare')
    if (result.details?.kind !== 'sign.prepare') {
      expect.unreachable('the payload is a signing plan')
    }
    expect(result.details.subjects[0]?.path).toBe('Acme-Windows-Setup.exe')
    expect(result.details.subjects[0]?.verified).toBe(false)
  })

  it('reads a signing failure per file', () => {
    const result = parseResult(fixture('sign-verify-failure'), 'sign.verify')
    expect(result.status).toBe('failure')
    if (result.details?.kind !== 'sign.verify') {
      expect.unreachable('the payload is a verification report')
    }
    expect(result.details.subjects[0]?.verified).toBe(false)
    expect(result.diagnostics[0]?.code).toBe('zup.signing.untrusted_chain')
  })

  it('tells an unsigned artifact from one nobody looked at', () => {
    const result = parseResult(fixture('sign-verify-failure'), 'sign.verify')
    // `unsigned` is a fact the platform established; `null` would be a fact nobody
    // established, and a summary that rendered them the same would be lying.
    expect(result.artifacts[0]?.signing?.state).toBe('unsigned')
    expect(isSigned(result.artifacts[0]!)).toBe(false)
  })

  it('reads an inspection whole, because the detail is the product', () => {
    const result = parseResult(fixture('artifact-inspect'), 'artifact.inspect')
    if (result.details?.kind !== 'artifact.inspect') {
      expect.unreachable('the payload is an inspection')
    }
    expect(result.details.artifact_kind).toBe('universal')
    expect(result.details.artifact_mode).toBe('offline')
    expect(result.details.content.stored_size).toBe(18_874_368)
    // A structural answer, not a trust answer: a linker emits no certificate table.
    expect(result.details.trust.authenticode).toBe('no certificate table')
    expect(result.details.trust.content_digests).toBe('valid')
  })

  it('reads a diagnostic that points at a line and a column', () => {
    const result = parseResult(fixture('diagnostic-span'), 'check')
    const source = result.diagnostics[0]?.source
    expect(source?.file).toBe('zup.toml')
    expect(source?.start_line).toBe(4)
    expect(source?.start_column).toBe(1)
    expect(source?.end_line).toBeNull()
  })

  it('reads a toolchain report with per-component state', () => {
    const result = parseResult(fixture('toolchain-status'), 'toolchain.status')
    if (result.details?.kind !== 'toolchain.status') {
      expect.unreachable('the payload is a toolchain report')
    }
    expect(result.details.complete).toBe(false)
    expect(result.details.components[0]?.found).toBe(false)
    expect(result.details.components[0]?.problem).toBeTruthy()
  })
})

describe('versions', () => {
  it('accepts the same major and refuses a different one', () => {
    expect(accepts('1.0')).toBe(true)
    expect(accepts('1.9')).toBe(true)
    expect(accepts('2.0')).toBe(false)
    expect(accepts('0.9')).toBe(false)
    expect(accepts('nonsense')).toBe(false)
  })

  it('refuses a document from a future major, and says which', () => {
    const future = { ...document('build-success'), protocol: '2.0' }
    expect(() => parseResult(JSON.stringify(future))).toThrow(
      new RegExp(`protocol is 2\\.0 and this action reads major ${PROTOCOL_MAJOR}`, 'u'),
    )
  })

  it('accepts a newer minor without complaint', () => {
    // The whole point of MAJOR.MINOR: a zup that added an event or a field in its
    // own minor is still readable, and refusing it would make the version
    // meaningless.
    const newer = { ...document('build-success'), protocol: '1.7' }
    expect(parseResult(JSON.stringify(newer)).operation).toBe('build')
  })

  it('ignores a field it has never heard of', () => {
    const richer = { ...document('build-success'), timing: { elapsed_ms: 91_000 } }
    const result = parseResult(JSON.stringify(richer))
    expect(result.status).toBe('success')
  })
})

describe('conformance', () => {
  it('refuses a failure that says nothing', () => {
    // Mutated after parsing, so the decoder is not the thing under test: it read a
    // valid document, and the document is the thing that is wrong.
    const silent = parseResult(fixture('build-success'), 'build')
    expect(conformance({ ...silent, status: 'failure' })).toMatch(/without saying why/u)
  })

  it('refuses a success carrying an error', () => {
    // A consumer that treats this as a success ships a broken release; one that
    // treats it as a failure has no exit code to back it up. Neither is right, so
    // the document is refused.
    const result = parseResult(fixture('build-failure'), 'build')
    expect(conformance({ ...result, status: 'success' })).toMatch(
      /reports success while carrying the error/u,
    )
  })

  it('refuses a payload for a different operation', () => {
    const result = parseResult(fixture('build-success'), 'build')
    const swapped = { ...result.details, kind: 'check' } as typeof result.details
    expect(conformance({ ...result, details: swapped })).toMatch(/carries the payload of `check`/u)
  })

  it('refuses a publication with no tag', () => {
    const result = parseResult(fixture('publish-success'), 'publish.github')
    expect(conformance({ ...result, publication: { ...result.publication!, tag: '  ' } })).toMatch(
      /no tag/u,
    )
  })

  it('accepts a warning on a success', () => {
    // The severity/status agreement the fixtures encode: a warning is something
    // to read, not a reason to fail.
    const warned = parseResult(fixture('check-not-composable'), 'check')
    expect(warned.diagnostics[0]?.severity).toBe('warning')
    expect(conformance(warned)).toBeUndefined()
  })
})

describe('tolerance', () => {
  it('treats an unknown severity as a notice', () => {
    // A level zup invented must not fail a release that otherwise succeeded.
    const result = parseResult(fixture('build-failure'), 'build')
    const invented = parseEvent(
      JSON.stringify({
        type: 'diagnostic',
        diagnostic: { ...result.diagnostics[0], severity: 'critical' },
      }),
    )
    if (invented?.type !== 'diagnostic') {
      expect.unreachable('the line is a diagnostic event')
    }
    expect(invented.diagnostic.severity).toBe('notice')
  })

  it('skips an event type from a newer zup rather than failing', () => {
    // Rule 2, on the wire: a consumer inside the same major keeps reading.
    expect(parseEvent('{"type":"cache_warm","blobs":2}')).toBeUndefined()
  })

  it('skips a line that is not an event at all', () => {
    expect(parseEvent('')).toBeUndefined()
    expect(parseEvent('   ')).toBeUndefined()
    expect(parseEvent('not json')).toBeUndefined()
    expect(parseEvent('[1,2,3]')).toBeUndefined()
    expect(parseEvent('{"no":"type"}')).toBeUndefined()
  })

  it('reads an artifact with no signing state as unlooked-at rather than unsigned', () => {
    const result = parseResult(fixture('build-success'), 'build')
    expect(result.artifacts[0]?.signing).toBeNull()
    expect(isSigned(result.artifacts[0]!)).toBe(false)
  })
})

describe('signing evidence', () => {
  it('is the fact that a signature covers the bytes, and nothing else', () => {
    expect(coversBytes([{ fact: 'signature_covers_bytes', value: 'sha256' }])).toBe(true)
    expect(coversBytes([{ fact: 'publisher', value: 'CN=Acme' }])).toBe(false)
    expect(coversBytes([])).toBe(false)
    expect(coversBytes(undefined)).toBe(false)
  })
})

describe('refusals', () => {
  it('refuses empty stdout', () => {
    expect(() => parseResult('   \n')).toThrow(ProtocolError)
    expect(() => parseResult('   \n')).toThrow(/stdout was empty/u)
  })

  it('refuses a document with no operation', () => {
    expect(() => parseResult('{"protocol":"1.0"}')).toThrow(/no `operation`/u)
  })

  it('refuses a result for a different operation', () => {
    // Running `zup build` and being handed a publish document would mean the
    // binary is not the one the action thinks it is.
    expect(() => parseResult(fixture('publish-success'), 'build')).toThrow(
      /expected a `build` result and got `publish.github`/u,
    )
  })

  it('refuses a truncated document and quotes what it saw', () => {
    try {
      parseResult(fixture('build-success').slice(0, 60))
      expect.unreachable('a truncated document must be refused')
    } catch (error) {
      expect((error as Error).message).toContain('did not emit a readable result')
      expect((error as Error).message).toContain('"protocol"')
    }
  })

  it('truncates a long preview rather than pasting a build log into an error', () => {
    // The message is the whole error, so a 5 KB stdout pasted into it would be a
    // log, and a log is what the reader already has.
    try {
      parseResult('x'.repeat(5000))
      expect.unreachable('a non-JSON stdout must be refused')
    } catch (error) {
      const message = (error as Error).message
      expect(message.length).toBeLessThan(400)
      expect(message).not.toContain('x'.repeat(300))
    }
  })
})

describe('the stream fixture', () => {
  it('is read line by line into the events it names', () => {
    const events = streamLines().map((line) => parseEvent(line))
    expect(events.every((event) => event !== undefined)).toBe(true)
    expect(events[0]?.type).toBe('version')
    expect(events.at(-1)?.type).toBe('completed')
  })

  it('names the operation on its first line, so a consumer can decide to keep reading', () => {
    const [first] = streamLines().map((line) => parseEvent(line))
    if (first?.type !== 'version') {
      expect.unreachable('the first line is the version line')
    }
    expect(first.operation).toBe('publish.stage')
    expect(first.protocol).toBe('1.0')
  })

  it('ends with a result a `--format json` run would have written', () => {
    // The stream's last line and the single document are the same document, so a
    // consumer that reported progress from the events and a summary from the
    // result cannot disagree with itself.
    const [event] = [parseEvent(streamLines().at(-1) ?? '')]
    if (event?.type !== 'completed') {
      expect.unreachable('the last line is the completed line')
    }
    expect(event.result.operation).toBe('publish.stage')
    expect(event.result.status).toBe('success')
    expect(event.result.details?.kind).toBe('publish.stage')
    // And it is a document the whole decoder accepts, rather than one that only
    // the streaming path happens to understand.
    expect(conformance(event.result)).toBeUndefined()
    expect(parseResult(JSON.stringify(event.result)).operation).toBe('publish.stage')
  })

  it('reassembles from arbitrary chunk boundaries', () => {
    // The adversarial case: a pipe splits wherever it likes, and a build that
    // takes ten minutes is read a hundred thousand times this way.
    const text = `${streamLines().join('\n')}\n`
    const events: string[] = []
    const framer = new LineFramer((line) => events.push(line))
    for (let index = 0; index < text.length; index += 7) {
      framer.push(new TextEncoder().encode(text.slice(index, index + 7)))
    }
    framer.end()
    expect(events).toEqual(streamLines())
    expect(events.at(-1)).toContain('"type":"completed"')
  })

  it('delivers a final line that arrived without a newline', () => {
    const events: string[] = []
    const framer = new LineFramer((line) => events.push(line))
    framer.push(new TextEncoder().encode('{"type":"log","level":"info","message":"last"}'))
    expect(events).toEqual([])
    framer.end()
    expect(events).toHaveLength(1)
    expect(parseEvent(events[0] ?? '')?.type).toBe('log')
  })

  it('decodes a multi-byte character split across two chunks', () => {
    // `chunk.toString('utf8')` per chunk would turn the middle of this into U+FFFD
    // and the line would stop parsing.
    const line = '{"type":"log","level":"info","message":"naïve ✅"}'
    const bytes = new TextEncoder().encode(line)
    const events: string[] = []
    const framer = new LineFramer((value) => events.push(value))
    for (let index = 0; index < bytes.length; index += 1) {
      framer.push(bytes.subarray(index, index + 1))
    }
    framer.end()
    expect(events).toHaveLength(1)
    const event = parseEvent(events[0] ?? '')
    if (event?.type !== 'log') {
      expect.unreachable('the line is a log event')
    }
    expect(event.message).toBe('naïve ✅')
  })
})
