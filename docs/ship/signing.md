# Signing

Zup coordinates signing but does not hold the signing credential.

The release flow is:

```bash
zup build
zup sign prepare --release-dir dist
# run your signer
zup sign verify --release-dir dist
```

## Prepare

`zup sign prepare` identifies the release files that need signatures and records the signing requirements.

A production pipeline can also bind the expected publisher identity:

```bash
zup sign prepare --release-dir dist --subject "Acme Labs"
```

The signer itself stays outside Zup. Use the signing service, certificate store or HSM already approved for the project.

## Verify

After signing:

```bash
zup sign verify --release-dir dist
```

Verification checks the signed files and finalizes the release description against the bytes that will actually be published.

## Unsigned internal builds

`zup sign verify --allow-unsigned` can finalize an unsigned development/internal release. Do not present that as equivalent to a publicly trusted Windows release; unsigned public installers will trigger normal Windows trust warnings.

## Keep credentials out of the manifest

`zup.toml` describes release policy, not secrets. Signing credentials belong in the external signing step or CI secret provider.
