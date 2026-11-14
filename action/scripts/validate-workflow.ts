import { readFileSync } from "node:fs";

function object(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Expected a mapping");
  }
  return Object.fromEntries(Object.entries(value));
}

function array(value: unknown): unknown[] {
  if (!Array.isArray(value)) throw new Error("Expected a sequence");
  return value;
}

function strings(value: unknown): string[] {
  if (value === undefined) return [];
  return (Array.isArray(value) ? value : [value]).map((entry: unknown) => {
    if (typeof entry !== "string") throw new Error("Expected a string");
    return entry;
  });
}

export function validateWorkflow(text: string): void {
  const workflow = object(Bun.YAML.parse(text));
  const jobs = object(workflow.jobs);
  const action = object(
    Bun.YAML.parse(
      readFileSync(new URL("../../action.yml", import.meta.url), "utf8"),
    ),
  );
  const inputs = object(action.inputs);
  const outputs = object(action.outputs);
  const producers = new Map<
    string,
    { job: string; finalized: boolean; path: string }
  >();
  const consumers: { job: string; name: string; path: string }[] = [];
  const ancestors = (
    name: string,
    visited = new Set<string>(),
  ): Set<string> => {
    if (visited.has(name)) throw new Error(`Cycle at ${name}`);
    visited.add(name);
    const result = new Set<string>();
    for (const need of strings(object(jobs[name]).needs)) {
      if (!(need in jobs)) throw new Error(`${name} needs absent job ${need}`);
      result.add(need);
      for (const ancestor of ancestors(need, new Set(visited)))
        result.add(ancestor);
    }
    return result;
  };
  for (const [name, rawJob] of Object.entries(jobs)) {
    const job = object(rawJob);
    ancestors(name);
    const steps = array(job.steps).map(object);
    const ids = new Map(
      steps
        .filter((step) => typeof step.id === "string")
        .map((step) => [String(step.id), step]),
    );
    if (ids.size !== steps.filter((step) => step.id !== undefined).length)
      throw new Error("Duplicate step ID");
    const references = JSON.stringify(job);
    for (const match of references.matchAll(
      /steps\.([\w-]+)\.outputs\.([\w-]+)/g,
    )) {
      const step = ids.get(match[1] ?? "");
      const output = match[2] ?? "";
      if (step === undefined)
        throw new Error(`Missing step producer ${match[0]}`);
      if (
        step.uses !== undefined
          ? !(output in outputs)
          : !String(step.run).includes(`${output}=`)
      ) {
        throw new Error(`Missing step output ${match[0]}`);
      }
    }
    for (const match of references.matchAll(
      /needs\.([\w-]+)\.outputs\.([\w-]+)/g,
    )) {
      const producer = match[1] ?? "";
      if (
        !strings(job.needs).includes(producer) ||
        !((match[2] ?? "") in object(object(jobs[producer]).outputs))
      ) {
        throw new Error(`Missing job output ${match[0]}`);
      }
    }
    let finalized = false;
    let builtTool = false;
    let pinnedSource: string | undefined;
    for (const step of steps) {
      if ((step.uses === undefined) === (step.run === undefined))
        throw new Error("Step must supply exactly one executable");
      const reference = String(step.uses ?? "");
      const parameters = step.with === undefined ? {} : object(step.with);
      if (
        reference.startsWith("actions/checkout@") &&
        parameters.path === ".zup-toolchain"
      ) {
        pinnedSource = `${String(parameters.repository)}@${String(parameters.ref)}`;
      }
      if (
        String(step.run).includes(
          "cargo build --locked --release -p zup --bin zup",
        )
      ) {
        if (
          pinnedSource === undefined ||
          step["working-directory"] !== ".zup-toolchain"
        )
          throw new Error("Publisher build has no pinned source directory");
        builtTool = true;
      }
      if (parameters["zup-path"] !== undefined) {
        if (!builtTool || reference !== pinnedSource)
          throw new Error(
            "Action and installed publisher must use the same pinned source",
          );
        for (const input of Object.keys(parameters))
          if (!(input in inputs))
            throw new Error(`Unknown action input ${input}`);
        if (parameters.operation === "finalize") finalized = true;
      }
      if (reference.startsWith("actions/upload-artifact@")) {
        const rawName = String(parameters.name);
        const names = rawName.includes("${{ matrix.profile }}")
          ? array(object(object(job.strategy).matrix).include).map((entry) =>
              rawName.replace(
                "${{ matrix.profile }}",
                String(object(entry).profile),
              ),
            )
          : [rawName];
        for (const artifact of names) {
          if (producers.has(artifact))
            throw new Error(`Duplicate artifact ${artifact}`);
          producers.set(artifact, {
            job: name,
            finalized,
            path: String(parameters.path),
          });
        }
      }
      if (
        reference.startsWith("orielhaim/zup@") &&
        parameters["upload-workflow-artifacts"] === true
      ) {
        const rawName = String(parameters["workflow-artifact-name"]);
        for (const entry of array(
          object(object(job.strategy).matrix).include,
        )) {
          const artifact = rawName.replace(
            "${{ matrix.profile }}",
            String(object(entry).profile),
          );
          if (producers.has(artifact))
            throw new Error(`Duplicate artifact ${artifact}`);
          producers.set(artifact, {
            job: name,
            finalized: false,
            path: String(parameters["release-dir"]).replace(
              "${{ matrix.profile }}",
              String(object(entry).profile),
            ),
          });
        }
      }
      if (reference.startsWith("actions/download-artifact@")) {
        if (parameters.pattern !== undefined)
          throw new Error("Publication requires exact artifact names");
        consumers.push({
          job: name,
          name: String(parameters.name),
          path: String(parameters.path),
        });
      }
    }
  }
  for (const consumer of consumers) {
    const producer = producers.get(consumer.name);
    if (producer === undefined || !ancestors(consumer.job).has(producer.job))
      throw new Error(`No upstream producer for ${consumer.name}`);
    if (producer.path !== consumer.path)
      throw new Error(`Artifact layout mismatch for ${consumer.name}`);
    if (["attest", "publish"].includes(consumer.job) && !producer.finalized)
      throw new Error("Unfinalized publication");
  }
  const publication = consumers.find((c) => c.job === "publish");
  const attestation = consumers.find((c) => c.job === "attest");
  if (
    attestation !== undefined &&
    (publication?.name !== attestation.name ||
      !ancestors("publish").has("attest"))
  )
    throw new Error("Publication and attestation disagree");
}

if (import.meta.main) {
  for (const path of process.argv.slice(2))
    validateWorkflow(readFileSync(path, "utf8"));
}
