# Signing a release

zup never holds a signing key. Not a PFX, not a password, not a token, not a
client secret, and nothing in this repository can produce a signature. The two
commands bracket your own signer instead.

That is the whole design. A tool that can sign is a tool whose compromise signs a
release, and the release is the only artifact a user's machine trusts without
reading it. Authenticode is a chain of trust to a root the machine already decides
about, and zup inserting itself into that chain would make the publisher's key and
the build tool's key the same trust decision.

```text
zup sign prepare  --release-dir <DIR> [--subject <SUBJECT>] [--thumbprint <HEX>]
                  [--allow-untrusted-chain] [--allow-missing-timestamp]
zup sign verify   --release-dir <DIR> [--report-only] [--allow-unsigned]
                  [--online-revocation]
```

`prepare` writes down what needs a signature and in what order. Your signer does
the signing. `verify` reads the result back, proves it, and rewrites the release
description with the identity that will actually be published. The default
`--release-dir` is `dist`.

## What is a signature, and who says so

A file can carry a signature structure, that structure can be internally
consistent, and the chain behind it can still be one the machine does not trust.
Three questions, three systems:

| Question | Asked by | Runs on |
| --- | --- | --- |
| Does the image carry a certificate table, and does the embedded digest cover these bytes? | `zup-pe` | any host |
| Which certificate made it, and does the structure carry a timestamp? | `zup-pe` | any host |
| Does this machine's trust policy accept the chain? | `zup-windows::signing` (`WinVerifyTrust`) | Windows |

A PE file has the same bytes on Linux as on Windows, so the structural half is a
fact about bytes and is read the same way everywhere - which is why a Linux build
host can answer it and why `zup-pe` builds and tests there. `zup-pe` decides
nothing about trust and has no opinion on whether a certificate is any good.

`WinVerifyTrust` consults the verifying machine's certificate store, which is a
statement about the machine rather than about the file, so it can only be made on
Windows. A non-Windows verifier records the first and does not pretend to the
second.

### The identity comes from the signature

The publisher is read out of the PKCS#7's own `SignerInfo` - issuer and serial
number - and matched against the certificate *inside the signature*. It is not
"the first certificate in the store that looks like a code-signing certificate":
on a timestamped signature the store's most plausible candidate is the timestamp
authority's, and reading the identity from there attributes a release to whoever
runs the timestamping service. There is no `crypt32` in this path.

## Order matters

A universal artifact embeds its runtime, so signing happens twice in a fixed
order:

```text
1. compose                        the runtime is still an ordinary PE
2. sign the runtime               (pre-compose)
3. compose again, embedding the *signed* runtime
4. sign the outer artifact        (post-compose)
5. finalize
```

Signing the outer file first produces an artifact whose embedded runtime is
unsigned. `zup sign verify` reads the embedded runtime resource back out of the
composed PE and requires it to hash to the digest the *signed* runtime claims. It
fails, and it fails on the property that matters rather than on a missing
signature.

That order is a type invariant, not a convention. `SigningPlan` only grows through
`push`, which inserts in signing order, and `validate()` re-checks the order on a
parsed document, so a plan whose steps are out of order does not parse. There is no
API for building one in the wrong order, and the JSON carries no crate or type
names - a pipeline reads it with `jq`.

## What `prepare` writes

A list of files, each with the role it plays (`native runtime`, `outer artifact`),
the stage it must be signed in (`pre-compose`, `post-compose`), and what to require
of the signature: the publisher's subject, the certificate thumbprint, and whether
an RFC 3161 timestamp is required.

The timestamp requirement is one decision, not two booleans. `required` and
`optional` are the only values, so "no timestamp required" and "refuse a legacy
timestamp" is a state the plan cannot express - and it never needed to be.

`--allow-untrusted-chain` and `--allow-missing-timestamp` exist because a
self-signed development certificate cannot chain to a root Windows trusts. They
change what `verify` *demands* and nothing else - never what is signed - and both
produce `SigningRequirement::development()`, a separate self-consistent set of
statements rather than a loosened production one, so no caller can weaken the
shipped requirement by accident.

## What `verify` checks

For every file the plan names:

1. **The signature covers these bytes.** `zup-pe` re-measures the file and
   compares; a signature that survives a byte change is a broken signature.
2. **The platform trusts the chain.** `WinVerifyTrust` is asked, and a trust
   failure is a failure.
3. **It is the right publisher.** The certificate's subject, and optionally its
   thumbprint, must match the plan. This is the check that stops *any*
   validly-signed file from being pasted into the release.
4. **It will still be valid later.** An expired certificate is still a signature,
   as long as an RFC 3161 countersignature was made while it was valid. The scan
   for the countersignature OID states what it proves and what it does not: that
   the structure carries a timestamp, not that the timestamp is from an authority
   you trust.

`--online-revocation` adds a CRL/OCSP check. It is off by default: a build host
without access to a revocation service must not fail an otherwise valid signature.

## Finalization: the published identity is a measurement

`verify` records the identity in the release description - the digest and size
**re-measured from the signed file** - and the evidence it established.

- **A published identity cannot be transcribed.** `FinalizedArtifact`'s fields are
  private; the only way to build one is `zup_signing::finalize()`, which measures
  the file. There is no constructor that takes a digest. (A *parsed* release
  description is the one exception, and a different thing: a publisher reading a
  manifest somebody else wrote is reading a claim, and re-measures before
  uploading.)
- **The measurement is checked twice.** `finalize` takes the measurement the
  caller took when it verified the signature and measures again. They must agree,
  or the file moved in between and neither describes the bytes on disk.

The result is a `SigningEvidence` list rather than a flag. A signed release can
carry five independently-failing facts - a signature covering the bytes, a chain
the platform accepted, a publisher, a certificate, a timestamp - and a
`Signed { .. }` enum would need a variant per platform and a field per fact, with
the fields a given platform cannot produce sitting in the document as `false`.

A build attestation, Sigstore provenance, TUF metadata, and a content-addressed
digest are supply-chain facts, not signatures *in the file*. They are recorded
elsewhere and deliberately kept out of `SigningEvidence`, so a release carrying an
attestation and no platform signature is still describable without pretending the
attestation was one.

## Unsigned is a real state

An unsigned release is a real release. Nothing changed the bytes, so the built
digest *is* the published digest, and the description records an empty evidence
list rather than claiming a signature. `--allow-unsigned` is how you get there,
and it is a deliberate act.

Unsigned is also *finalized*: it has a real published identity, and nothing proves
who produced it. That is exactly why the evidence is a list and not a boolean -
"finalized" and "signed" are different questions about the same block, and a
boolean invites reading one as the other.

```text
zup sign verify --allow-unsigned     # finalizes as unsigned
zup publish github                   # warns loudly by name, and still refuses an unfinalized release
```

A release that reaches the internet unsigned is one Windows SmartScreen will warn
about.

## Publishing

`publish` refuses a release that is not finalized, by name. A description of
pre-sign bytes describes bytes nobody can obtain, and uploading it would put a
manifest on the internet that no download can satisfy.

It measures each artifact's digest and size itself and cross-checks both against
the description, so a description that does not match the file is refused rather
than published.

## In CI

The generated workflow has two shapes, depending on whether the project has a
signing step.

**With signing**, `compose` uploads its unsigned artifact, the `sign` job downloads
it, signs, runs `zup sign verify`, and uploads `compose`; `attest` and `publish`
depend on `[compose, sign]` and collect the **signed** tree.

**Without signing**, `compose` finalizes in place with `--allow-unsigned` and
uploads once - no redundant multi-gigabyte round trip through a second job for a
tree nobody will re-sign.

The shape matters because an earlier generator had `compose` upload its artifact,
`sign` run afterwards, and both `attest` and `publish` collect the **unsigned**
tree: the release was attested and published with pre-sign bytes while every log
said it was signed.

In the Action, `finalize` is the half that runs `zup sign verify`, and it reads the
release description rather than parsing a command's report, so the summary
reports the identity a downloader will actually get. `action/src/artifacts.ts`
**refuses a manifest with no `finalized` identity**, and attestation re-derives
each subject's digest from the bytes. Both are the same rule the CLI enforces, in
the place a pipeline cannot route around it. See [action.md](action.md).

## What zup does not do

- It does not hold, request, or store a key.
- It does not call a signing service.
- It does not produce a certificate, a CSR, or a timestamp token.
- It does not decide whether a publisher is trustworthy; it decides whether the
  bytes carry the signature that publisher's key made, and re-measures the bytes
  rather than believing the record.
