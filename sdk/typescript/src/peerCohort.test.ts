// @vitest-environment node

import { describe, expect, it, vi } from "vitest";
import type { OxyConfig } from "./config";
import { PeerCohortClient } from "./peerCohort";

// client.ts pulls in parquet.ts, which imports `@duckdb/duckdb-wasm` — a
// dependency declared by web-app, not by this package, and not resolvable
// from sdk/typescript's own node_modules. Stub it out so importing OxyClient
// here doesn't fail on that unrelated, pre-existing phantom dependency.
vi.mock("./parquet", () => ({ readParquet: vi.fn() }));

const { OxyClient } = await import("./client");

function makeClient() {
  const config: OxyConfig = {
    apiKey: "test-key",
    projectId: "proj-123",
    baseUrl: "https://api.test",
    timeout: 5000
  };
  const request = vi.fn(async (_endpoint: string, _options?: RequestInit) => ({}) as unknown);
  return { client: new PeerCohortClient(config, request as never), request, config };
}

describe("PeerCohortClient", () => {
  it("resolve POSTs the request body verbatim to the cohort endpoint", async () => {
    const { client, request } = makeClient();
    const req = {
      entity: "stores.store",
      measure: "orders.net_revenue",
      time_dimension: "orders.order_date",
      period: ["2025-09-01", "2025-09-30"] as [string, string]
    };
    await client.resolve(req);
    expect(request).toHaveBeenCalledWith(
      "/proj-123/semantic/cohort",
      expect.objectContaining({ method: "POST", body: JSON.stringify(req) })
    );
  });

  it("resolve forwards optional cohort and statistic untouched", async () => {
    const { client, request } = makeClient();
    const req = {
      entity: "stores.store",
      measure: "orders.net_revenue",
      time_dimension: "orders.order_date",
      period: ["2025-09-01", "2025-09-30"] as [string, string],
      cohort: "same_region",
      statistic: "p75"
    };
    await client.resolve(req);
    expect(request).toHaveBeenCalledWith(
      "/proj-123/semantic/cohort",
      expect.objectContaining({ method: "POST", body: JSON.stringify(req) })
    );
  });

  it("appends branch to the query string when configured", async () => {
    const config: OxyConfig = {
      apiKey: "k",
      projectId: "p",
      baseUrl: "https://api.test",
      branch: "feature/x",
      timeout: 5000
    };
    const request = vi.fn(async () => ({}) as unknown);
    const client = new PeerCohortClient(config, request as never);
    const req = {
      entity: "stores.store",
      measure: "orders.net_revenue",
      time_dimension: "orders.order_date",
      period: ["2025-09-01", "2025-09-30"] as [string, string]
    };
    await client.resolve(req);
    expect(request).toHaveBeenCalledWith(
      "/p/semantic/cohort?branch=feature%2Fx",
      expect.objectContaining({ method: "POST", body: JSON.stringify(req) })
    );
  });
});

describe("OxyClient.peerCohort", () => {
  it("is lazily constructed and memoised — repeated access returns the same instance", () => {
    const config: OxyConfig = {
      apiKey: "test-key",
      projectId: "proj-123",
      baseUrl: "https://api.test",
      timeout: 5000
    };
    const client = new OxyClient(config);
    const first = client.peerCohort;
    expect(first).toBeInstanceOf(PeerCohortClient);
    expect(client.peerCohort).toBe(first);
  });
});
