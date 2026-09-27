// The entry point, and nothing else. The implementation is in `workflow.ts` so a test
// can import it without running it; this file exists to report a rejection as a failed
// step rather than an unhandled rejection on stderr.

import * as core from '@actions/core'

import { run } from './workflow.js'

void run().catch((error: unknown) => {
  core.setFailed(error instanceof Error ? error.message : String(error))
})
