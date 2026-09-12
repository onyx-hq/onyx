/**
 * Target resolution — a port of the Rust `env_url.rs` (since deleted).
 *
 * Every case below is transcribed from that module's own tests, and that is
 * the point: `oxy` and `oxyc` share a credentials file **keyed by host**, so if
 * the two disagree about which host an `--env` names, each caches a token under
 * a key the other cannot find and both report the user as logged out. Nothing
 * at compile time notices; only this does.
 */

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import {
  defaultTarget,
  loadForTargetResolution,
  loadManifest,
  looksLikeUrl,
  type OxyAppManifest,
  parseEnvUrl,
  resolveEnv
} from "./target.js";

/** The Rust's `resolve()` helper, so the cases below read the same. */
const resolve = (value: string) => {
  const r = parseEnvUrl(value);
  if (!r) throw new Error(`did not parse: ${value}`);
  return r;
};

describe("looksLikeUrl", () => {
  it("leaves env names as env names", () => {
    for (const name of ["production", "prod", "dev", "staging", "local", "my_env"]) {
      expect(looksLikeUrl(name), `${name} must stay an env name`).toBe(false);
    }
    expect(looksLikeUrl("")).toBe(false);
  });

  it("recognises a URL with or without a scheme", () => {
    for (const v of [
      "https://app.oxygen-hq.com",
      "http://localhost:3000",
      "app.oxygen-hq.com",
      "localhost:3000"
    ]) {
      expect(looksLikeUrl(v), `${v} must read as a URL`).toBe(true);
    }
  });
});

describe("parseEnvUrl", () => {
  it("resolves a product host to itself, with no org", () => {
    expect(resolve("https://app.oxygen-hq.com")).toEqual({ target: "https://app.oxygen-hq.com" });
    expect(resolve("https://aip.staging.oxy.tech")).toEqual({
      target: "https://aip.staging.oxy.tech"
    });
    expect(resolve("https://aip.dev.oxy.tech")).toEqual({ target: "https://aip.dev.oxy.tech" });
  });

  /**
   * The canonicalisation that makes one login per DEPLOYMENT rather than one
   * per customer: every org subdomain points at the same product host.
   */
  it("canonicalises an org subdomain and yields the slug", () => {
    expect(resolve("https://poke-house.oxygen-hq.com")).toEqual({
      target: "https://app.oxygen-hq.com",
      orgSlug: "poke-house"
    });
    expect(resolve("https://poke-house.staging.oxy.tech")).toEqual({
      target: "https://aip.staging.oxy.tech",
      orgSlug: "poke-house"
    });
  });

  /**
   * A custom-app host also ends in the org zone, and its label carries a `--`
   * pair the org rule would mis-read as the whole slug. It must yield the ORG,
   * never the app.
   */
  it("yields the org, not the app, from a custom-app subdomain", () => {
    expect(resolve("https://poke-house--bookkeeping.customer-apps.oxygen-hq.com")).toEqual({
      target: "https://app.oxygen-hq.com",
      orgSlug: "poke-house"
    });
  });

  /** What people paste is a PAGE, not an API base. */
  it("drops the path, query and fragment of a pasted page URL", () => {
    expect(resolve("https://app.oxygen-hq.com/threads/abc?x=1#y")).toEqual({
      target: "https://app.oxygen-hq.com"
    });
    expect(resolve("https://poke-house.oxygen-hq.com/apps/sales")).toEqual({
      target: "https://app.oxygen-hq.com",
      orgSlug: "poke-house"
    });
  });

  it("treats an unknown host as its own target", () => {
    expect(resolve("https://oxy.acme.internal:8443/ide")).toEqual({
      target: "https://oxy.acme.internal:8443"
    });
  });

  it("gives loopback http when the scheme is omitted", () => {
    expect(resolve("localhost:3000")).toEqual({ target: "http://localhost:3000" });
    expect(resolve("http://127.0.0.1:5173")).toEqual({ target: "http://127.0.0.1:5173" });
  });

  /** The port split must not chop `[::1]` in half. */
  it("survives a bracketed IPv6 loopback", () => {
    expect(resolve("[::1]:3000")).toEqual({ target: "http://[::1]:3000" });
  });

  /**
   * `a.b.oxygen-hq.com` is not the org-subdomain shape, so it must be neither
   * read as an org NOR canonicalised away to the product host.
   */
  it("does not turn a multi-label prefix into an org slug", () => {
    expect(resolve("https://a.b.oxygen-hq.com")).toEqual({ target: "https://a.b.oxygen-hq.com" });
  });

  it("normalises host case and a trailing dot", () => {
    expect(resolve("https://Poke-House.OXYGEN-HQ.com.")).toEqual({
      target: "https://app.oxygen-hq.com",
      orgSlug: "poke-house"
    });
  });

  /** A reserved label is the product host itself, and implies no org. */
  it("reads a reserved label as the product host", () => {
    expect(resolve("https://www.oxygen-hq.com").orgSlug).toBeUndefined();
    expect(resolve("https://api.oxygen-hq.com").orgSlug).toBeUndefined();
  });
});

describe("defaultTarget", () => {
  /**
   * `local` is the VITE dev server on 5173, not oxy's own 3000, and that is
   * load-bearing: `oxyc login` opens `<target>/cli-auth`, a route that exists
   * only in the live web app, while `oxy serve` serves a pre-built bundle that
   * may predate it.
   */
  it("points local at the Vite dev server, not oxy's own port", () => {
    expect(defaultTarget("local")).toBe("http://localhost:5173");
  });

  it("knows the deployments, and only those", () => {
    expect(defaultTarget("dev")).toBe("https://aip.dev.oxy.tech");
    expect(defaultTarget("staging")).toBe("https://aip.staging.oxy.tech");
    expect(defaultTarget("production")).toBe("https://app.oxygen-hq.com");
    expect(defaultTarget("prod")).toBe("https://app.oxygen-hq.com");
    expect(defaultTarget("nonsense")).toBeUndefined();
  });
});

describe("resolveEnv precedence", () => {
  const manifest = { environments: { dev: { target: "https://custom.example.com" } } };

  it("lets --target win outright", () => {
    expect(resolveEnv("production", "https://forced.example.com", manifest)?.target).toBe(
      "https://forced.example.com"
    );
  });

  it("keeps --target verbatim, for a deployment served under a path", () => {
    expect(resolveEnv(undefined, "https://host.example.com/oxy")?.target).toBe(
      "https://host.example.com/oxy"
    );
  });

  it("still mines the org slug from --target", () => {
    expect(resolveEnv(undefined, "https://poke-house.oxygen-hq.com")?.orgSlug).toBe("poke-house");
  });

  it("prefers the manifest over the built-in default", () => {
    expect(resolveEnv("dev", undefined, manifest)?.target).toBe("https://custom.example.com");
  });

  it("falls back to the built-in when the manifest has no entry", () => {
    expect(resolveEnv("staging", undefined, manifest)?.target).toBe("https://aip.staging.oxy.tech");
  });

  /**
   * The URL reading is LAST and purely additive. That ordering is why
   * `--env local` never tries to resolve `local` as a hostname.
   */
  it("reads a URL env only after every name has failed", () => {
    expect(resolveEnv("https://oxy.acme.internal", undefined)?.target).toBe(
      "https://oxy.acme.internal"
    );
    expect(resolveEnv("local", undefined)?.target).toBe("http://localhost:5173");
  });

  it("returns undefined when nothing resolves", () => {
    expect(resolveEnv("", undefined)).toBeUndefined();
    expect(resolveEnv(undefined, undefined)).toBeUndefined();
  });

  it("trims a trailing slash so the host key cannot differ by one character", () => {
    expect(resolveEnv(undefined, "https://app.oxygen-hq.com/")?.target).toBe(
      "https://app.oxygen-hq.com"
    );
  });
});

/** A throwaway directory holding `oxy-app.json` with these exact contents. */
function withManifest(contents: string, check: (dir: string) => void): void {
  const dir = mkdtempSync(join(tmpdir(), "oxyc-target-"));
  try {
    writeFileSync(join(dir, "oxy-app.json"), contents);
    check(dir);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

const BROKEN = ['{ "slug": "x", ', '{ "slug": 5 }', "[]", '{ "environments": { "dev": "x" } }'];

describe("loadManifest", () => {
  it("returns undefined rather than throwing when there is no manifest", () => {
    expect(loadManifest("/nonexistent-directory-for-a-test")).toBeUndefined();
  });

  /**
   * It used to answer `undefined` for these too, and every caller then acted
   * as if there were no manifest: `publish` bundled no functions and took its
   * identity from flags or the directory, without a word.
   */
  it("throws, naming the file, for one that exists but is not a valid manifest", () => {
    for (const bad of BROKEN) {
      withManifest(bad, (dir) => {
        expect(() => loadManifest(dir), bad).toThrow(join(dir, "oxy-app.json"));
      });
    }
  });

  it("lets unknown fields and nulls through", () => {
    withManifest('{ "slug": "x", "orgSlug": null, "someFutureKey": [1] }', (dir) => {
      expect(loadManifest(dir)?.slug).toBe("x");
    });
  });
});

describe("an environments entry without a target", () => {
  /** Newer manifests put other per-environment keys under `environments`. */
  it("resolves as if the entry were absent", () => {
    const manifest: OxyAppManifest = {
      environments: {
        dev: {},
        production: { target: "   " },
        "https://poke-house.oxygen-hq.com": {}
      }
    };
    for (const env of ["dev", "production"]) {
      expect(resolveEnv(env, undefined, manifest)?.target, env).toBe(defaultTarget(env));
    }
    expect(resolveEnv("https://poke-house.oxygen-hq.com", undefined, manifest)).toEqual({
      target: "https://app.oxygen-hq.com",
      orgSlug: "poke-house"
    });
  });
});

describe("loadForTargetResolution", () => {
  it("skips a broken manifest when --target decides, and is strict when it does not", () => {
    withManifest('{ "slug": "x", ', (dir) => {
      expect(loadForTargetResolution(dir, "https://flag.example.com")).toBeUndefined();
      for (const flag of [undefined, "", "   "]) {
        expect(() => loadForTargetResolution(dir, flag), String(flag)).toThrow("oxy-app.json");
      }
    });
    withManifest('{ "slug": "x" }', (dir) => {
      expect(loadForTargetResolution(dir, undefined)?.slug).toBe("x");
    });
  });
});
