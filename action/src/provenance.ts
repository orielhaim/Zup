export interface ReleaseSource {
  commit: string;
  repository: string;
}

function record(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Provenance requires a source and build definition");
  }
  return Object.fromEntries(Object.entries(value));
}

export function releaseSource(value: unknown): ReleaseSource {
  const source = record(value);
  if (
    typeof source["commit"] !== "string" ||
    !/^[0-9a-f]{40}$/u.test(source["commit"]) ||
    typeof source["repository"] !== "string" ||
    !/^[A-Za-z0-9.-]+\/[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/u.test(
      source["repository"],
    )
  ) {
    throw new Error(
      "Attestation requires the recorded build repository and full commit SHA",
    );
  }
  return { commit: source["commit"], repository: source["repository"] };
}

export function withReleaseSource(
  predicate: object,
  source: ReleaseSource,
): object {
  const params = record(predicate);
  const definition = record(params["buildDefinition"]);
  if (!Array.isArray(definition["resolvedDependencies"])) {
    throw new Error("The SLSA predicate has no resolved dependencies");
  }
  return {
    ...params,
    buildDefinition: {
      ...definition,
      resolvedDependencies: [
        ...definition["resolvedDependencies"],
        {
          uri: `git+https://${source["repository"]}@${source["commit"]}`,
          digest: { gitCommit: source["commit"] },
        },
      ],
    },
  };
}
