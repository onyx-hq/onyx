/**
 * The proxy's guardrails.
 *
 * These are the whole reason the command exists: everything else is a
 * forwarder. Each predicate is a decision carried over verbatim from the Rust
 * `oxy proxy` this replaced, and getting one wrong means a laptop writing to a
 * customer's production data — a failure nobody notices until it has already
 * happened.
 */

import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { ExitCode } from "../util/errors.js";

const BIN = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "dist", "main.mjs");

import {
  buildRequestHeaders,
  ensureTraceparent,
  functionRoute,
  isAuthPath,
  isEventsPath,
  isProductionTarget,
  isWritePath,
  rewriteSetCookie,
  traceIdOf
} from "./proxy.js";

/**
 * A port the OS says is free, rather than one derived from another port.
 *
 * The previous `45_000 + upstreamPort % 1000` was a guess: anything else on the
 * machine holding that port made the proxy fail to bind, and the readiness loop
 * below then spent its full budget before failing with a connection error that
 * named nothing. Binding zero and reading the number back leaves only the race
 * between close and re-bind, which is orders of magnitude narrower.
 */
async function freePort(): Promise<number> {
  const probe = createServer();
  await new Promise<void>((r) => probe.listen(0, "127.0.0.1", r));
  const { port } = probe.address() as AddressInfo;
  await new Promise<void>((r) => probe.close(() => r()));
  return port;
}

describe("isWritePath — held by default", () => {
  it("holds every mutating method", () => {
    for (const method of ["POST", "PUT", "PATCH", "DELETE"]) {
      expect(isWritePath(method, "/api/w/apps"), method).toBe(true);
    }
  });

  it("never holds a read", () => {
    expect(isWritePath("GET", "/api/w/apps")).toBe(false);
    expect(isWritePath("HEAD", "/api/w/apps")).toBe(false);
  });

  /**
   * The two POST-but-READ endpoints. They carry their filter in the body, so
   * they are POSTs that change nothing — holding them would break the one
   * thing a custom app in `pnpm dev` actually does.
   */
  it("lets the data-plane POSTs through", () => {
    expect(isWritePath("POST", "/api/projects/abc/query")).toBe(false);
    expect(isWritePath("POST", "/api/projects/abc/semantic-query")).toBe(false);
  });

  /** …but only those two, and only as a whole final segment. */
  it("still holds something that merely ends in a similar word", () => {
    expect(isWritePath("POST", "/api/w/apps/requery")).toBe(true);
    expect(isWritePath("POST", "/api/w/fn/run")).toBe(true);
  });

  it("is not fooled by a query string", () => {
    expect(isWritePath("POST", "/api/projects/abc/query?debug=1")).toBe(false);
    expect(isWritePath("POST", "/api/w/apps?x=1")).toBe(true);
  });

  it("leaves events and auth to their own handling", () => {
    expect(isWritePath("POST", "/api/customer-apps/abc/events")).toBe(false);
    expect(isWritePath("POST", "/api/auth/callback")).toBe(false);
  });
});

describe("isEventsPath — dropped by default", () => {
  it("matches the tracking endpoint", () => {
    expect(isEventsPath("/api/customer-apps/abc/events")).toBe(true);
    expect(isEventsPath("/api/customer-apps/abc/events?x=1")).toBe(true);
  });

  it("does not match a different customer-apps route", () => {
    expect(isEventsPath("/api/customer-apps/abc/manifest")).toBe(false);
    expect(isEventsPath("/api/w/events")).toBe(false);
  });
});

describe("isAuthPath — must reach the backend unauthenticated", () => {
  it("covers the auth tree and the user probe", () => {
    expect(isAuthPath("/api/auth/callback")).toBe(true);
    expect(isAuthPath("/api/auth/dev-login?email=x")).toBe(true);
    expect(isAuthPath("/api/user")).toBe(true);
  });

  /** `/api/users` is a different thing entirely. */
  it("matches /api/user exactly, not as a prefix", () => {
    expect(isAuthPath("/api/users")).toBe(false);
    expect(isAuthPath("/api/user/settings")).toBe(false);
  });
});

describe("buildRequestHeaders — the token is a fallback, never an override", () => {
  it("injects the dev bearer when the request carries no auth", () => {
    const h = buildRequestHeaders({}, "dev-token", "/api/w/apps");
    expect(h.authorization).toBe("Bearer dev-token");
  });

  /**
   * A real browser session must win. Overriding it would silently act as a
   * different user than the one signed in, which is the worst possible way to
   * be wrong about authorization.
   */
  it("leaves a browser cookie alone", () => {
    const h = buildRequestHeaders({ cookie: "session=abc" }, "dev-token", "/api/w/apps");
    expect(h.authorization).toBeUndefined();
    expect(h.cookie).toBe("session=abc");
  });

  it("leaves an explicit Authorization alone", () => {
    const h = buildRequestHeaders({ authorization: "Bearer theirs" }, "dev-token", "/api/w/apps");
    expect(h.authorization).toBe("Bearer theirs");
  });

  /** Injecting on an auth endpoint would break sign-in. */
  it("never injects on an auth endpoint", () => {
    expect(
      buildRequestHeaders({}, "dev-token", "/api/auth/callback").authorization
    ).toBeUndefined();
    expect(buildRequestHeaders({}, "dev-token", "/api/user").authorization).toBeUndefined();
  });

  /**
   * The backend derives its base URL from these, and an OAuth `redirect_uri`
   * that does not match what the provider issued the code for is a 401.
   */
  it("forwards Origin and Referer, which sign-in depends on", () => {
    const h = buildRequestHeaders(
      { origin: "http://localhost:5173", referer: "http://localhost:5173/app" },
      undefined,
      "/api/auth/callback"
    );
    expect(h.origin).toBe("http://localhost:5173");
    expect(h.referer).toBe("http://localhost:5173/app");
  });
});

describe("functionRoute — only an Oxy Function call is annotated", () => {
  it("names the function of a /customer-apps/<org>/<slug>/fn/<name> call", () => {
    expect(functionRoute("/customer-apps/acme/sales/fn/refresh")).toBe("refresh");
    expect(functionRoute("/customer-apps/o/s/fn/sync/extra")).toBe("sync");
    expect(functionRoute("/customer-apps/o/s/fn/refresh?debug=1")).toBe("refresh");
  });

  /** A bundle asset that merely contains `/fn/` is not a call. */
  it("leaves assets and other routes alone", () => {
    expect(functionRoute("/customer-apps/acme/sales/assets/app.js")).toBeUndefined();
    expect(functionRoute("/customer-apps/acme/sales/assets/fn/x.js")).toBeUndefined();
    expect(functionRoute("/customer-apps/acme/sales/fn")).toBeUndefined();
    expect(functionRoute("/api/fn/not-an-app")).toBeUndefined();
  });
});

describe("traceparent — one trace per function call", () => {
  /**
   * The SDK's header must survive the outbound allowlist, or the id the page
   * stamped on its error names a trace that exists nowhere.
   */
  it("forwards the SDK's traceparent and tracestate untouched", () => {
    const traceparent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    const headers = buildRequestHeaders(
      { traceparent, tracestate: "oxy=1" },
      undefined,
      "/customer-apps/o/s/fn/refresh"
    );
    expect(ensureTraceparent(headers)).toBe("0af7651916cd43dd8448eb211c80319c");
    expect(headers.traceparent).toBe(traceparent);
    expect(headers.tracestate).toBe("oxy=1");
  });

  it("mints a sampled one when the call arrived without", () => {
    const headers: Record<string, string> = {};
    const minted = ensureTraceparent(headers);
    expect(minted).toMatch(/^[0-9a-f]{32}$/);
    expect(traceIdOf(headers.traceparent ?? "")).toBe(minted);
    expect(headers.traceparent).toMatch(/-01$/);
  });

  /** A rejected header takes its `tracestate` with it (W3C). */
  it("replaces a malformed one rather than forwarding it, and drops its tracestate", () => {
    for (const bad of [
      "garbage",
      "00-00000000000000000000000000000000-b7ad6b7169203331-01",
      "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01",
      "zz-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
      "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331"
    ]) {
      const headers: Record<string, string> = { traceparent: bad, tracestate: "vendor=stale" };
      const minted = ensureTraceparent(headers);
      expect(bad.includes(minted), bad).toBe(false);
      expect(traceIdOf(headers.traceparent ?? ""), bad).toBe(minted);
      expect(headers.tracestate, bad).toBeUndefined();
    }
  });
});

describe("rewriteSetCookie — storable on localhost", () => {
  it("strips Domain and Secure, which stop a browser storing it on http://localhost", () => {
    const out = rewriteSetCookie("session=abc; Domain=.oxygen-hq.com; Secure; HttpOnly; Path=/");
    expect(out).not.toMatch(/domain=/i);
    expect(out).not.toMatch(/\bsecure\b/i);
    expect(out).toContain("session=abc");
    expect(out).toContain("HttpOnly");
    expect(out).toContain("Path=/");
  });

  /** Browsers only honour `SameSite=None` together with `Secure`, which just went. */
  it("relaxes SameSite=None, which cannot survive without Secure", () => {
    expect(rewriteSetCookie("s=1; SameSite=None; Secure")).toContain("SameSite=Lax");
  });

  it("leaves an already-storable cookie alone", () => {
    expect(rewriteSetCookie("s=1; Path=/; HttpOnly")).toBe("s=1; Path=/; HttpOnly");
  });
});

describe("the forwarder, end to end", () => {
  let upstream: Server;
  let base: string;
  let proxy: ChildProcess;
  let proxyUrl: string;
  let proxyStderr = "";
  let streamClosed = false;

  beforeAll(async () => {
    upstream = createServer((req, res) => {
      const path = (req.url ?? "/").split("?")[0];
      if (path === "/customer-apps/o/s/fn/refresh") {
        res.writeHead(200, { "content-type": "application/json", "x-oxy-request-id": "req-123" });
        res.end(JSON.stringify({ traceparent: req.headers.traceparent ?? null }));
        return;
      }
      if (path === "/api/broken-stream") {
        // Headers and a first chunk, then the connection dies — the status is
        // already on the wire when the forwarder finds out.
        res.writeHead(200, { "content-type": "text/plain" });
        res.write("partial");
        setTimeout(() => res.socket?.destroy(), 20);
        return;
      }
      if (path === "/api/stream") {
        // An SSE stream that never ends on its own. `close` fires only if the
        // connection goes away, which is the thing under test.
        res.writeHead(200, { "content-type": "text/event-stream" });
        const tick = setInterval(() => res.write("data: tick\n\n"), 25);
        res.on("close", () => {
          clearInterval(tick);
          streamClosed = true;
        });
        return;
      }
      if (path === "/api/gzipped") {
        // Compressed on the wire. undici asks for this and decodes it, so the
        // forwarder must not pass `content-encoding` on to the browser.
        const body = gzipSync(Buffer.from(JSON.stringify({ big: "x".repeat(2000) })));
        res.writeHead(200, { "content-type": "application/json", "content-encoding": "gzip" });
        res.end(body);
        return;
      }
      if (path === "/api/cookie") {
        res.writeHead(200, {
          "content-type": "application/json",
          "set-cookie": ["a=1; Domain=.oxygen-hq.com; Secure; Path=/", "b=2; Path=/"]
        });
        res.end("{}");
        return;
      }
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          method: req.method,
          path: req.url,
          auth: req.headers.authorization ?? null
        })
      );
    });
    await new Promise<void>((r) => upstream.listen(0, "127.0.0.1", r));
    base = `http://127.0.0.1:${(upstream.address() as AddressInfo).port}`;

    const port = await freePort();
    proxyUrl = `http://127.0.0.1:${port}`;
    proxy = spawn(process.execPath, [BIN, "proxy", "--target", base, "--port", String(port)], {
      env: { ...process.env, OXY_TOKEN: "dev-token", NO_COLOR: "1" },
      stdio: ["ignore", "ignore", "pipe"]
    });
    proxy.stderr?.on("data", (d) => {
      proxyStderr += d;
    });

    // Wait for the listener rather than sleeping a fixed amount.
    let ready = false;
    for (let i = 0; i < 100; i++) {
      try {
        await fetch(`${proxyUrl}/api/ping`);
        ready = true;
        break;
      } catch {
        await new Promise((r) => setTimeout(r, 100));
      }
    }
    // FAIL HERE, NAMING WHY. Without this every case below fails on its own
    // fetch with a bare connection error, and the actual reason — the proxy
    // exited, or could not bind — is in a stream nobody reads.
    if (!ready) {
      throw new Error(
        `the proxy never came up on ${proxyUrl}` +
          (proxyStderr ? `\n--- its stderr ---\n${proxyStderr}` : " (it printed nothing)")
      );
    }
  }, 30_000);

  afterAll(() => {
    proxy?.kill();
    upstream.close();
  });

  /**
   * The case the command exists for. node's fetch decompresses transparently,
   * so forwarding `content-encoding: gzip` on the decoded bytes is
   * `ERR_CONTENT_DECODING_FAILED` in the browser.
   */
  it("does not label a decompressed body as still compressed", async () => {
    const res = await fetch(`${proxyUrl}/api/gzipped`);
    expect(res.headers.get("content-encoding")).toBeNull();
    // …and the body really is readable, which is the point.
    expect(((await res.json()) as { big: string }).big).toHaveLength(2000);
  });

  /**
   * `@oxy-hq/sdk` matches `^403:` as its catch-all and renders "Access denied
   * — check the oxy server logs", which would discard the explanation this
   * response carries. 409 has no branch there.
   */
  it("answers a held write 409, not 403, so the reason survives the SDK", async () => {
    const res = await fetch(`${proxyUrl}/api/w/apps`, { method: "POST", body: "{}" });
    expect(res.status).toBe(409);
    expect(((await res.json()) as { message: string }).message).toMatch(/--allow-writes/);
  });

  it("forwards a read, injecting the dev bearer", async () => {
    const body = (await (await fetch(`${proxyUrl}/api/ping`)).json()) as { auth: string };
    expect(body.auth).toBe("Bearer dev-token");
  });

  it("lets the POST-but-read data-plane endpoints through", async () => {
    const res = await fetch(`${proxyUrl}/api/projects/x/query`, { method: "POST", body: "{}" });
    expect(res.status).toBe(200);
  });

  it("drops a tracking event with a 204 rather than an error", async () => {
    const res = await fetch(`${proxyUrl}/api/customer-apps/x/events`, {
      method: "POST",
      body: "{}"
    });
    expect(res.status).toBe(204);
  });

  /** `Headers.forEach` comma-joins Set-Cookie; a login setting two would lose one. */
  it("keeps multiple Set-Cookie headers separate, and rewrites each", async () => {
    const res = await fetch(`${proxyUrl}/api/cookie`);
    const cookies = res.headers.getSetCookie();
    expect(cookies).toHaveLength(2);
    expect(cookies.join(" ")).not.toMatch(/domain=/i);
    expect(cookies.join(" ")).not.toMatch(/\bsecure\b/i);
  });

  /**
   * The line a developer copies into HyperDX after watching a function fail:
   * the trace the proxy sent upstream, and the request id the server answered
   * with, so neither has to be dug out by hand.
   */
  it("gives a function call a trace and prints its ids", async () => {
    const res = await fetch(`${proxyUrl}/customer-apps/o/s/fn/refresh`);
    const sent = ((await res.json()) as { traceparent: string | null }).traceparent;
    const traceId = traceIdOf(sent ?? "");
    expect(traceId).toMatch(/^[0-9a-f]{32}$/);
    for (let i = 0; i < 20 && !proxyStderr.includes(`trace_id=${traceId}`); i++) {
      await new Promise((r) => setTimeout(r, 50));
    }
    expect(proxyStderr).toContain(`↳ 200 fn refresh  request_id=req-123  trace_id=${traceId}`);
  });

  /**
   * Last-but-one, and on purpose: before the fix this took the proxy process
   * down, which would fail every case after it for a reason none of them name.
   */
  it("survives an upstream that dies after the status is sent", async () => {
    await fetch(`${proxyUrl}/api/broken-stream`, { signal: AbortSignal.timeout(5_000) })
      .then((r) => r.text())
      .catch(() => undefined);
    expect(proxy.exitCode, proxyStderr).toBeNull();
    const res = await fetch(`${proxyUrl}/api/ping`, { signal: AbortSignal.timeout(5_000) });
    expect(res.status).toBe(200);
  });

  /** An SSE stream nobody reads must not stay open against the cloud. */
  it("closes the upstream request when the browser goes away", async () => {
    const browser = new AbortController();
    const res = await fetch(`${proxyUrl}/api/stream`, { signal: browser.signal });
    await res.body?.getReader().read();
    browser.abort();
    for (let i = 0; i < 30 && !streamClosed; i++) {
      await new Promise((r) => setTimeout(r, 100));
    }
    expect(streamClosed).toBe(true);
  });
});

describe("isProductionTarget — refused without --yes", () => {
  it("catches the product host and every org subdomain under it", () => {
    for (const target of [
      "https://app.oxygen-hq.com",
      "https://acme.oxygen-hq.com",
      "https://poke-house.oxygen-hq.com",
      "https://acme--app.customer-apps.oxygen-hq.com"
    ]) {
      expect(isProductionTarget(target), target).toBe(true);
    }
  });

  /**
   * THE CASE A SUFFIX TEST LOSES. `"oxygen-hq.com".endsWith(".oxygen-hq.com")`
   * is false, so a guard written as the suffix alone waves the apex through —
   * which the regex this predicate replaced did catch. Pinned separately from
   * the subdomains above because it is the one host the obvious rewrite drops.
   */
  it("catches the apex, which a bare suffix test does not", () => {
    expect(isProductionTarget("https://oxygen-hq.com")).toBe(true);
    expect(isProductionTarget("https://oxygen-hq.com/some/path")).toBe(true);
    expect(isProductionTarget("https://OXYGEN-HQ.COM")).toBe(true);
  });

  it("leaves dev, staging, loopback and a self-hosted host alone", () => {
    for (const target of [
      "https://aip.dev.oxy.tech",
      "https://aip.staging.oxy.tech",
      "https://poke-house.staging.oxy.tech",
      "http://localhost:3000",
      "http://127.0.0.1:5173",
      "https://oxy.acme.internal:8443"
    ]) {
      expect(isProductionTarget(target), target).toBe(false);
    }
  });

  /**
   * A host that merely ENDS in the same letters is not under the zone —
   * `notoxygen-hq.com` would match a naive `endsWith("oxygen-hq.com")`.
   */
  it("is not fooled by a lookalike host", () => {
    expect(isProductionTarget("https://notoxygen-hq.com")).toBe(false);
    expect(isProductionTarget("https://oxygen-hq.com.evil.test")).toBe(false);
  });

  /** "I could not tell" is not a reason to allow an accident. */
  it("treats an unparseable target as production", () => {
    expect(isProductionTarget("not a url")).toBe(true);
  });
});

describe("the production refusal, through the binary", () => {
  /**
   * A bare `oxyc proxy` defaults to production, so it must refuse.
   *
   * The port is arbitrary and never bound — the refusal happens before the
   * listener is opened, which is the property this pins.
   */
  it("refuses without --yes and names the target", () => {
    const r = spawnSync(process.execPath, [BIN, "proxy", "--port", "45999"], {
      encoding: "utf8",
      env: { ...process.env, OXY_TOKEN: "t", NO_COLOR: "1" },
      timeout: 15_000
    });
    expect(r.status).toBe(ExitCode.REFUSED);
    expect(r.stderr).toMatch(/production target/);
    expect(r.stderr).toMatch(/--yes/);
  });
});
