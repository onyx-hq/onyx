/**
 * The hand-written tar, checked against the system `tar` — an independent
 * reader, and the same family (libarchive / GNU) the server's unpacker agrees
 * with on these features.
 */

import { spawnSync } from "node:child_process";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { CliError } from "../util/errors.js";
import { paxPathRecord, tarGzDir } from "./tarball.js";

let root: string;
beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "oxyc-tar-"));
});
afterEach(() => rmSync(root, { recursive: true, force: true }));

function extract(archive: Buffer): string {
  const file = join(root, "bundle.tar.gz");
  const dest = join(root, "extracted");
  mkdirSync(dest);
  writeFileSync(file, archive);
  const result = spawnSync("tar", ["-xzf", file, "-C", dest], { encoding: "utf8" });
  expect(result.status, result.stderr).toBe(0);
  return dest;
}

function list(archive: Buffer): string[] {
  const file = join(root, "list.tar.gz");
  writeFileSync(file, archive);
  const result = spawnSync("tar", ["-tzf", file], { encoding: "utf8" });
  expect(result.status, result.stderr).toBe(0);
  return result.stdout.split("\n").filter(Boolean).sort();
}

describe("tarGzDir", () => {
  it("round-trips files, nested directories and dotfiles at the archive root", () => {
    const src = join(root, "out");
    mkdirSync(join(src, "assets", "img"), { recursive: true });
    mkdirSync(join(src, ".vite"));
    writeFileSync(join(src, "index.html"), "<html></html>");
    writeFileSync(join(src, "assets", "img", "logo.svg"), "<svg/>");
    writeFileSync(join(src, ".vite", "manifest.json"), "{}");
    writeFileSync(join(src, "empty.txt"), "");
    const binary = Buffer.from(Array.from({ length: 1300 }, (_, i) => i % 256));
    writeFileSync(join(src, "assets", "blob.bin"), binary);

    const archive = tarGzDir(src);
    expect(list(archive)).toEqual([
      ".vite/",
      ".vite/manifest.json",
      "assets/",
      "assets/blob.bin",
      "assets/img/",
      "assets/img/logo.svg",
      "empty.txt",
      "index.html"
    ]);
    const dest = extract(archive);
    expect(readFileSync(join(dest, "index.html"), "utf8")).toBe("<html></html>");
    expect(readFileSync(join(dest, ".vite", "manifest.json"), "utf8")).toBe("{}");
    expect(readFileSync(join(dest, "assets", "blob.bin")).equals(binary)).toBe(true);
    expect(readFileSync(join(dest, "empty.txt"), "utf8")).toBe("");
  });

  /** Past ustar's 100-byte name field: the PAX `path` record carries it. */
  it("keeps a path longer than 100 bytes, including a multibyte one", () => {
    const src = join(root, "out");
    const deep = join(src, "a".repeat(60), "b".repeat(60));
    mkdirSync(deep, { recursive: true });
    const name = `${"é".repeat(30)}.js`;
    writeFileSync(join(deep, name), "long");

    const dest = extract(tarGzDir(src));
    expect(readFileSync(join(dest, "a".repeat(60), "b".repeat(60), name), "utf8")).toBe("long");
  });

  /** A link entry would unpack server-side as an empty file. */
  it("follows symlinks, to files and to directories", () => {
    const outside = join(root, "shared");
    mkdirSync(join(outside, "lib"), { recursive: true });
    writeFileSync(join(outside, "lib", "util.js"), "export {}");
    writeFileSync(join(outside, "robots.txt"), "User-agent: *");
    const src = join(root, "out");
    mkdirSync(src);
    symlinkSync(join(outside, "robots.txt"), join(src, "robots.txt"));
    symlinkSync(join(outside, "lib"), join(src, "lib"));

    const dest = extract(tarGzDir(src));
    expect(readFileSync(join(dest, "robots.txt"), "utf8")).toBe("User-agent: *");
    expect(readFileSync(join(dest, "lib", "util.js"), "utf8")).toBe("export {}");
  });

  it("visits a directory symlinked back up the tree only once", () => {
    const src = join(root, "out");
    mkdirSync(join(src, "sub"), { recursive: true });
    writeFileSync(join(src, "sub", "a.txt"), "a");
    symlinkSync(src, join(src, "sub", "loop"));
    expect(list(tarGzDir(src))).toEqual(["sub/", "sub/a.txt"]);
  });

  it("keeps the executable bit", () => {
    const src = join(root, "out");
    mkdirSync(src);
    writeFileSync(join(src, "run.sh"), "#!/bin/sh\n");
    chmodSync(join(src, "run.sh"), 0o755);
    const dest = extract(tarGzDir(src));
    expect(statSync(join(dest, "run.sh")).mode & 0o111).not.toBe(0);
  });

  it("refuses a directory that does not exist", () => {
    expect(() => tarGzDir(join(root, "nope"))).toThrow(CliError);
  });
});

describe("paxPathRecord", () => {
  /** The length prefix counts its own digits, which can carry it to one more digit. */
  it("counts the whole record, including the length itself", () => {
    for (const size of [1, 80, 91, 92, 93, 990, 994, 995]) {
      const record = paxPathRecord("p".repeat(size));
      const declared = Number(record.toString().split(" ")[0]);
      expect(declared, `path of ${size}`).toBe(record.length);
    }
  });
});
