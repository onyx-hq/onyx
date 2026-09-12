/**
 * `oxyc publish`, end to end: the built binary against a fake deployment.
 *
 * The fake answers the three routes a publish calls plus the trusted-publishing
 * pair, and records what arrived, so each case asserts on the REQUEST — the
 * fields, the bundle's contents, which credential — rather than on the prose.
 */

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";

import { ExitCode } from "../util/errors.js";
import { inferOrgApp } from "./publish.js";

const BIN = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "dist", "main.mjs");

interface Received {
  path: string;
  authorization?: string;
  fields: Record<string, string>;
  bundle?: Buffer;
  exchangeBody?: string;
}

let server: Server;
let target: string;
let received: Received[] = [];
/** What the fake answers the upload with. */
let uploadStatus = 200;

async function body(req: IncomingMessage): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(chunk as Buffer);
  return Buffer.concat(chunks);
}

beforeAll(async () => {
  server = createServer(async (req, res) => {
    const path = req.url ?? "/";
    const raw = await body(req);
    const record: Received = { path, authorization: req.headers.authorization, fields: {} };
    received.push(record);
    const reply = (status: number, value: unknown) => {
      res.writeHead(status, { "content-type": "application/json" });
      res.end(JSON.stringify(value));
    };

    if (path === "/api/apps/acme/sales/build-config") return reply(200, { project_id: "proj-1" });
    if (path.startsWith("/api/apps/")) return reply(404, { error: "not found" });
    if (path === "/api/org-for-project/proj-9") return reply(200, { org_slug: "acme" });
    if (path.startsWith("/github-oidc")) {
      return req.headers.authorization === "bearer gh-request-token" &&
        path.includes("audience=oxy-publish")
        ? reply(200, { value: "gh-jwt" })
        : reply(403, {});
    }
    if (path === "/api/customer-apps/publish/oidc-exchange") {
      record.exchangeBody = raw.toString();
      return req.headers.authorization === "Bearer gh-jwt"
        ? reply(200, { token: "minted", expires_at: "later" })
        : reply(401, { error: "invalid OIDC token" });
    }
    if (path === "/api/customer-apps/publish") {
      const form = await new Request("http://x", {
        method: "POST",
        headers: { "content-type": req.headers["content-type"] ?? "" },
        body: raw
      }).formData();
      for (const [name, value] of form.entries()) {
        if (typeof value === "string") record.fields[name] = value;
        else record.bundle = Buffer.from(await value.arrayBuffer());
      }
      const token = req.headers.authorization;
      if (token !== "Bearer good-token" && token !== "Bearer minted") {
        return reply(403, { error: "not an app admin" });
      }
      if (uploadStatus !== 200) return reply(uploadStatus, { error: "nope" });
      return reply(200, {
        app_id: "app-1",
        build_id: record.fields.build_id,
        url: "/customer-apps/acme/sales/",
        channel: record.fields.channel,
        org_slug: "acme",
        is_new_app: false,
        warnings: ["schedule for fn refresh did not register"]
      });
    }
    reply(404, { error: `unexpected ${path}` });
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  target = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
});

afterAll(() => {
  server.close();
});

let work: string;
beforeEach(() => {
  work = mkdtempSync(join(tmpdir(), "oxyc-publish-"));
  received = [];
  uploadStatus = 200;
});
afterEach(() => rmSync(work, { recursive: true, force: true }));

/** An app directory with a manifest and a pre-built `out/`. */
function app(manifest: Record<string, unknown> = { slug: "sales", orgSlug: "acme" }): string {
  const dir = join(work, "app");
  mkdirSync(join(dir, "out"), { recursive: true });
  writeFileSync(join(dir, "oxy-app.json"), JSON.stringify(manifest));
  writeFileSync(join(dir, "out", "index.html"), "<html>sales</html>");
  return dir;
}

/**
 * The binary, with an environment built from nothing: a runner's own
 * `GITHUB_*` / `ACTIONS_*` would otherwise leak into the build id and the auth
 * decision, and the real credentials file must never be read.
 */
async function publish(
  cwd: string,
  args: string[],
  env: Record<string, string> = {}
): Promise<{ status: number | null; stdout: string; stderr: string }> {
  const child = spawn(process.execPath, [BIN, "publish", "--target", target, ...args], {
    cwd,
    env: {
      PATH: process.env.PATH ?? "",
      HOME: work,
      OXY_CREDENTIALS_PATH: join(work, "credentials.json"),
      NO_COLOR: "1",
      ...env
    }
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (d) => {
    stdout += d;
  });
  child.stderr.on("data", (d) => {
    stderr += d;
  });
  const status = await new Promise<number | null>((r) => child.on("close", r));
  return { status, stdout, stderr };
}

function uploads(): Received[] {
  return received.filter((r) => r.path === "/api/customer-apps/publish");
}

function tarList(bundle: Buffer): string[] {
  const file = join(work, "sent.tar.gz");
  writeFileSync(file, bundle);
  return spawnSync("tar", ["-tzf", file], { encoding: "utf8" }).stdout.split("\n").filter(Boolean);
}

describe("oxyc publish", () => {
  it("publishes a pre-built bundle as a draft with the fields the server reads", async () => {
    const dir = app();
    const result = await publish(dir, ["--dir", "out", "--build-id", "b-1"], {
      OXY_TOKEN: "good-token"
    });
    expect(result.status, result.stderr).toBe(0);

    const [upload] = uploads();
    expect(upload?.authorization).toBe("Bearer good-token");
    expect(upload?.fields).toMatchObject({
      app: "sales",
      org: "acme",
      project: "proj-1",
      build_id: "b-1",
      channel: "draft"
    });
    expect(tarList(upload?.bundle ?? Buffer.alloc(0))).toEqual(["index.html"]);
    expect(result.stdout).toContain("Published new version of acme/sales (id app-1)");
    // The server's warnings, which the Rust client dropped.
    expect(result.stderr).toContain("schedule for fn refresh did not register");
  });

  it("prints the server's result as JSON on stdout with --json, and promotes", async () => {
    const dir = app();
    const result = await publish(dir, ["--dir", "out", "--json", "--promote"], {
      OXY_TOKEN: "good-token"
    });
    expect(result.status, result.stderr).toBe(0);
    const parsed = JSON.parse(result.stdout) as { channel: string; warnings: string[] };
    expect(parsed.channel).toBe("published");
    expect(parsed.warnings).toHaveLength(1);
  });

  it("builds from source with the app's base path exported", async () => {
    const dir = app({
      slug: "sales",
      orgSlug: "acme",
      build: {
        install: "true",
        command: 'mkdir -p dist && printf %s "$OXY_APP_BASE_PATH" > dist/index.html',
        outDir: "dist"
      }
    });
    const result = await publish(dir, [], { OXY_TOKEN: "good-token" });
    expect(result.status, result.stderr).toBe(0);
    const file = join(work, "check.tar.gz");
    writeFileSync(file, uploads()[0]?.bundle ?? Buffer.alloc(0));
    const html = spawnSync("tar", ["-xzOf", file, "index.html"], { encoding: "utf8" }).stdout;
    expect(html).toBe("/customer-apps/acme/sales/");
  });

  /** Before the build: a missing login costs a second, not a two-minute install. */
  it("refuses without a credential before running the build", async () => {
    const dir = app({
      slug: "sales",
      orgSlug: "acme",
      build: { install: "touch installed", command: "true" }
    });
    const result = await publish(dir, []);
    expect(result.status).toBe(ExitCode.AUTH);
    expect(result.stderr).toContain("oxyc login");
    expect(existsSync(join(dir, "installed"))).toBe(false);
    expect(uploads()).toHaveLength(0);
  });

  /** Publishing past it used to ship the bundle with no functions and a guessed identity. */
  it("stops on a broken oxy-app.json before building or uploading", async () => {
    const dir = app();
    writeFileSync(join(dir, "oxy-app.json"), '{ "slug": "sales", "orgSlug": "acme", ');
    const result = await publish(dir, ["--dir", "out"], { OXY_TOKEN: "good-token" });
    expect(result.status).toBe(ExitCode.USAGE);
    expect(result.stderr).toContain("is not a valid oxy-app.json");
    expect(uploads()).toHaveLength(0);
  });

  it("maps a 403 to the auth exit code with the server's body", async () => {
    const dir = app();
    const result = await publish(dir, ["--dir", "out"], { OXY_TOKEN: "wrong-token" });
    expect(result.status).toBe(ExitCode.AUTH);
    expect(result.stderr).toContain("not an app admin");
    expect(result.stderr).toContain("oxyc whoami");
  });

  it("maps a duplicate build id to the request exit code", async () => {
    uploadStatus = 409;
    const dir = app();
    const result = await publish(dir, ["--dir", "out"], { OXY_TOKEN: "good-token" });
    expect(result.status).toBe(ExitCode.REQUEST);
  });

  it("names an unregistered app as not found", async () => {
    const dir = app({ slug: "other", orgSlug: "acme" });
    const result = await publish(dir, ["--dir", "out"], { OXY_TOKEN: "good-token" });
    expect(result.status).toBe(ExitCode.NOT_FOUND);
    expect(result.stderr).toContain("--project");
  });

  /** A pinned workspace determines its org, so the manifest need not name one. */
  it("resolves the org from a pinned project", async () => {
    const dir = app({ slug: "sales" });
    const result = await publish(dir, ["--dir", "out", "--project", "proj-9"], {
      OXY_TOKEN: "good-token"
    });
    expect(result.status, result.stderr).toBe(0);
    expect(uploads()[0]?.fields).toMatchObject({ org: "acme", project: "proj-9" });
  });

  it("uses GitHub's run as the build id and falls back to its ref for the branch", async () => {
    const dir = app();
    const result = await publish(dir, ["--dir", "out"], {
      OXY_TOKEN: "good-token",
      GITHUB_SHA: "abc123",
      GITHUB_RUN_ID: "42",
      GITHUB_RUN_ATTEMPT: "2",
      GITHUB_REF_NAME: "main"
    });
    expect(result.status, result.stderr).toBe(0);
    expect(uploads()[0]?.fields).toMatchObject({
      build_id: "abc123-42.2",
      commit_sha: "abc123",
      branch: "main"
    });
    // A commit with no repo is half a record — said out loud.
    expect(result.stderr).toContain("no git repo");
  });

  describe("trusted publishing", () => {
    const oidcEnv = () => ({
      ACTIONS_ID_TOKEN_REQUEST_URL: `${target}/github-oidc?api-version=2.0`,
      ACTIONS_ID_TOKEN_REQUEST_TOKEN: "gh-request-token"
    });

    it("exchanges the job's OIDC token for a credential scoped to the app", async () => {
      const dir = app();
      const result = await publish(dir, ["--dir", "out"], oidcEnv());
      expect(result.status, result.stderr).toBe(0);
      const exchange = received.find((r) => r.path.endsWith("/oidc-exchange"));
      expect(JSON.parse(exchange?.exchangeBody ?? "{}")).toEqual({ app: "acme/sales" });
      expect(uploads()[0]?.authorization).toBe("Bearer minted");
    });

    /** A set token is the operator's explicit choice; OIDC is only the fallback. */
    it("prefers a token that is set", async () => {
      const dir = app();
      const result = await publish(dir, ["--dir", "out"], {
        ...oidcEnv(),
        OXY_TOKEN: "good-token"
      });
      expect(result.status, result.stderr).toBe(0);
      expect(received.some((r) => r.path.endsWith("/oidc-exchange"))).toBe(false);
    });

    /** The exchange is registered by slug; a UUID cannot be matched. */
    it("needs the org slug", async () => {
      const dir = app({ slug: "sales" });
      const result = await publish(
        dir,
        ["--dir", "out", "--org", "0b0e5a10-1111-4222-8333-944455556666", "--project", "proj-1"],
        oidcEnv()
      );
      expect(result.status).toBe(ExitCode.USAGE);
      expect(result.stderr).toContain("org SLUG");
      expect(uploads()).toHaveLength(0);
    });
  });

  describe("split CI jobs", () => {
    it("--build-only builds and bundles without a credential or the network", async () => {
      const dir = app({
        slug: "sales",
        orgSlug: "acme",
        build: { install: "true", command: "mkdir -p out && echo built > out/index.html" }
      });
      const result = await publish(dir, ["--build-only", "--json"]);
      expect(result.status, result.stderr).toBe(0);
      // Resolved: the child's cwd is the real path, and a temp dir is often a symlink.
      expect(JSON.parse(result.stdout)).toEqual({ bundle_dir: join(realpathSync(dir), "out") });
      expect(received).toHaveLength(0);
    });

    /** The server does not check, so a missing function would 404 at runtime. */
    it("--prebuilt refuses a bundle missing a declared function", async () => {
      const dir = app({ slug: "sales", orgSlug: "acme", functions: { refresh: {} } });
      const result = await publish(dir, ["--dir", "out", "--prebuilt"], {
        OXY_TOKEN: "good-token"
      });
      expect(result.status).toBe(ExitCode.USAGE);
      expect(result.stderr).toContain("functions/refresh.js");
      expect(uploads()).toHaveLength(0);
    });

    it("--prebuilt publishes the functions it finds, without running esbuild", async () => {
      const dir = app({ slug: "sales", orgSlug: "acme", functions: { refresh: {} } });
      mkdirSync(join(dir, "out", "functions"));
      writeFileSync(join(dir, "out", "functions", "refresh.js"), "export default () => 1");
      const result = await publish(dir, ["--dir", "out", "--prebuilt"], {
        OXY_TOKEN: "good-token",
        // No pnpm on PATH: bundling would fail, so success proves it did not run.
        PATH: dirname(process.execPath)
      });
      expect(result.status, result.stderr).toBe(0);
      expect(tarList(uploads()[0]?.bundle ?? Buffer.alloc(0)).sort()).toEqual([
        "functions/",
        "functions/refresh.js",
        "index.html"
      ]);
    });

    it("--prebuilt needs --dir", async () => {
      const result = await publish(app(), ["--prebuilt"], { OXY_TOKEN: "good-token" });
      expect(result.status).toBe(ExitCode.USAGE);
    });
  });
});

describe("inferOrgApp", () => {
  it("reads apps/<org>/<app> from the working directory", () => {
    expect(inferOrgApp("/r/apps/acme/sales")).toEqual({ org: "acme", app: "sales" });
    expect(inferOrgApp("/r/apps/acme/sales/src")).toEqual({ org: "acme", app: "sales" });
    expect(inferOrgApp("/r/elsewhere")).toEqual({});
  });
});
