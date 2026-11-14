import { expect, test } from "bun:test";
import { mkdtemp, mkdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { workflowArtifactFiles } from "../src/runtime.js";

test("the toolkit receives every regular file with the declared archive root", async () => {
  const root = await mkdtemp(join(tmpdir(), "zup-transfer-"));
  try {
    await mkdir(join(root, "packages"));
    await writeFile(join(root, "zup-release.json"), "manifest");
    await writeFile(join(root, "packages", "package.zup.000"), "first");
    await writeFile(join(root, "packages", "package.zup.001"), "second");
    expect(
      (await workflowArtifactFiles([root], root)).map((file) =>
        relative(root, file).replaceAll("\\", "/"),
      ),
    ).toEqual([
      "packages/package.zup.000",
      "packages/package.zup.001",
      "zup-release.json",
    ]);
  } finally {
    await rm(root, { recursive: true });
  }
});

test("foreign paths and linked directories are rejected before artifact upload", async () => {
  const root = await mkdtemp(join(tmpdir(), "zup-transfer-"));
  const outside = await mkdtemp(join(tmpdir(), "zup-foreign-"));
  try {
    await writeFile(join(outside, "credential"), "private fixture");
    await expect(
      workflowArtifactFiles([join(outside, "credential")], root),
    ).rejects.toThrow();
    await symlink(
      outside,
      join(root, "linked"),
      process.platform === "win32" ? "junction" : "dir",
    );
    await expect(workflowArtifactFiles([root], root)).rejects.toThrow();
  } finally {
    await rm(root, { recursive: true });
    await rm(outside, { recursive: true });
  }
});
