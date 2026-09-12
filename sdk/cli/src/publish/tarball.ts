/**
 * The bundle as a `.tar.gz`, entries at the archive root.
 *
 * WRITTEN HERE rather than taken from a tar package. The format needed is
 * small — POSIX ustar headers, plus a PAX `path` record for a name over 100
 * bytes — and `oxyc` also ships as a Bun-compiled binary, where a dependency
 * that reaches into Node's zlib internals can work under every test (which
 * run on Node) and fail only in the binary. `gzipSync` is public API on both.
 *
 * Symlinks are FOLLOWED and archived as their target's contents, as the Rust
 * client did: a link entry would unpack server-side as an empty file.
 */

import { readdirSync, readFileSync, realpathSync, statSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";

import { CliError, ExitCode } from "../util/errors.js";

const BLOCK = 512;

function octal(value: number, width: number): string {
  return `${value.toString(8).padStart(width - 1, "0")}\0`;
}

/** One 512-byte header block. `name` is written raw, truncated to 100 bytes. */
function header(name: Buffer, size: number, mode: number, mtime: number, type: string): Buffer {
  const block = Buffer.alloc(BLOCK);
  name.copy(block, 0, 0, Math.min(name.length, 100));
  block.write(octal(mode, 8), 100, "ascii");
  block.write(octal(0, 8), 108, "ascii");
  block.write(octal(0, 8), 116, "ascii");
  block.write(octal(size, 12), 124, "ascii");
  block.write(octal(mtime, 12), 136, "ascii");
  block.write(" ".repeat(8), 148, "ascii");
  block.write(type, 156, "ascii");
  block.write("ustar\0", 257, "ascii");
  block.write("00", 263, "ascii");
  let sum = 0;
  for (const byte of block) sum += byte;
  block.write(`${sum.toString(8).padStart(6, "0")}\0 `, 148, "ascii");
  return block;
}

/** `<len> path=<path>\n`, where `<len>` counts the whole record including itself. */
export function paxPathRecord(path: string): Buffer {
  const body = ` path=${path}\n`;
  const bodyLength = Buffer.byteLength(body);
  let length = bodyLength + String(bodyLength).length;
  if (String(length).length !== String(bodyLength).length)
    length = bodyLength + String(length).length;
  return Buffer.from(`${length}${body}`);
}

function padding(size: number): Buffer {
  const rest = size % BLOCK;
  return Buffer.alloc(rest === 0 ? 0 : BLOCK - rest);
}

interface Entry {
  path: string;
  kind: "file" | "dir";
  source: string;
}

/**
 * Every entry under `root`, directories before their contents, sorted so the
 * same tree always produces the same archive. A symlinked directory that
 * points back up the tree is visited once.
 */
function walk(root: string): Entry[] {
  const entries: Entry[] = [];
  const seen = new Set<string>([realpathSync(root)]);
  const visit = (dir: string, prefix: string): void => {
    for (const name of readdirSync(dir).sort()) {
      const source = join(dir, name);
      const path = prefix ? `${prefix}/${name}` : name;
      const stat = statSync(source);
      if (stat.isDirectory()) {
        const real = realpathSync(source);
        if (seen.has(real)) continue;
        seen.add(real);
        entries.push({ path: `${path}/`, kind: "dir", source });
        visit(source, path);
      } else if (stat.isFile()) {
        entries.push({ path, kind: "file", source });
      }
    }
  };
  visit(root, "");
  return entries;
}

function entryBlocks(entry: Entry): Buffer[] {
  const stat = statSync(entry.source);
  const mtime = Math.floor(stat.mtimeMs / 1000);
  const name = Buffer.from(entry.path);
  const blocks: Buffer[] = [];
  if (name.length > 100) {
    const record = paxPathRecord(entry.path);
    blocks.push(header(Buffer.from("PaxHeader"), record.length, 0o644, mtime, "x"));
    blocks.push(record, padding(record.length));
  }
  if (entry.kind === "dir") {
    blocks.push(header(name, 0, 0o755, mtime, "5"));
    return blocks;
  }
  const content = readFileSync(entry.source);
  const mode = stat.mode & 0o111 ? 0o755 : 0o644;
  blocks.push(header(name, content.length, mode, mtime, "0"), content, padding(content.length));
  return blocks;
}

/** Tar and gzip the contents of `dir`. Throws when it is not a directory. */
export function tarGzDir(dir: string): Buffer {
  let isDir = false;
  try {
    isDir = statSync(dir).isDirectory();
  } catch {
    isDir = false;
  }
  if (!isDir) {
    throw new CliError(`bundle dir ${dir} does not exist (did the build run?)`, {
      code: ExitCode.USAGE
    });
  }
  const blocks = walk(dir).flatMap(entryBlocks);
  blocks.push(Buffer.alloc(BLOCK * 2));
  return gzipSync(Buffer.concat(blocks));
}
