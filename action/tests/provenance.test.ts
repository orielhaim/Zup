import { expect, test } from "bun:test";
import { releaseSource, withReleaseSource } from "../src/provenance.js";

test("manual dispatch retains workflow provenance and records the distinct built commit", () => {
  const source = releaseSource({
    commit: "a".repeat(40),
    repository: "github.com/acme/app",
  });
  const event = {
    uri: "git+https://github.com/acme/app@refs/heads/main",
    digest: { gitCommit: "b".repeat(40) },
  };
  const workflow = { ref: "refs/heads/main" };
  const result = withReleaseSource(
    {
      buildDefinition: {
        resolvedDependencies: [event],
        externalParameters: { workflow },
      },
      runDetails: { builder: "workflow" },
    },
    source,
  );
  expect(result).toEqual({
    buildDefinition: {
      externalParameters: { workflow },
      resolvedDependencies: [
        event,
        {
          uri: `git+https://github.com/acme/app@${"a".repeat(40)}`,
          digest: { gitCommit: "a".repeat(40) },
        },
      ],
    },
    runDetails: { builder: "workflow" },
  });
});

test.each([
  undefined,
  {},
  { commit: "main", repository: "github.com/acme/app" },
  { commit: "a".repeat(40), repository: "https://token@example.invalid/repo" },
])("invalid source is refused before attestation", (value) => {
  expect(() => releaseSource(value)).toThrow();
});
