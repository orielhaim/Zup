# Static hosting

Zup can stage a release as a static file tree:

```bash
zup publish stage --release-dir dist
```

Serve the resulting tree from an HTTP origin, CDN or object store. The origin does not need Zup-specific server logic.

## Configure clients

`[updates].repository` identifies the repository URL clients use. `[distribution]` controls how published release content is hosted.

Keep those URLs stable. They become part of the release/update contract for installed clients.

## What belongs on the origin

Publish the complete staged output. Do not hand-pick individual metadata or blob files from it.

Do not edit a staged release tree after publishing it. Publish a new release or channel update instead.

## Static vs GitHub

Use a static origin when you want CDN/object-store control or private publishing infrastructure with anonymous client reads. Use GitHub release assets when the project is public and a separate origin would add no value.
