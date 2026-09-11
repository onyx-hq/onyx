import { describe, expect, it } from "vitest";
import { describeDevLoginFailure, serverReason } from "./describeDevLoginFailure";

// Every one of these refusals used to be invisible: the page held its spinner
// forever because the mutation's `status` never reached the surviving render.
// The copy is the only thing that tells a developer which knob to turn, so it
// is worth pinning that each status maps to its own actionable sentence.
describe("describeDevLoginFailure", () => {
  it("tells you the bypass is off when the server 404s", () => {
    const msg = describeDevLoginFailure(404, "dev@oxy.local");
    expect(msg).toContain("not enabled");
    expect(msg).toContain("OXY_DEV_LOGIN_EMAILS");
    // Order matters: the explicit var is the whole fix on a release binary
    // (`oxy start`, a Docker image), where the debug-only roster fallback is
    // inert — so it must lead, with the fallback as the caveat behind it.
    expect(msg.indexOf("OXY_DEV_LOGIN_EMAILS")).toBeLessThan(msg.indexOf("OXY_GLOBAL_ADMINS"));
  });

  it("names the rejected identity on a 403", () => {
    expect(describeDevLoginFailure(403, "nope@nowhere.test")).toContain('"nope@nowhere.test"');
  });

  it("stays grammatical on a 403 with no email supplied", () => {
    const msg = describeDevLoginFailure(403, undefined);
    expect(msg).not.toContain('""');
    expect(msg).toContain("not listed");
  });

  it("explains a deleted user on a 401", () => {
    expect(describeDevLoginFailure(401, "gone@oxy.tech")).toContain("deleted");
  });

  it("falls back to a generic message for anything else", () => {
    expect(describeDevLoginFailure(500, "dev@oxy.local")).toContain("server logs");
    expect(describeDevLoginFailure(undefined, undefined)).toContain("server logs");
  });

  // `?as=` refusals: the server's reason names the fix (valid personas, or the
  // seed command), so it wins over any copy baked in here.
  it("shows the server's reason for an unseeded persona", () => {
    const reason = "persona `member` (amara.larsson@acme.test) is not seeded — run `just up`";
    expect(describeDevLoginFailure(409, undefined, reason)).toBe(reason);
    expect(describeDevLoginFailure(409, undefined)).toContain("oxy seed");
  });

  it("shows the server's reason for a bad persona request", () => {
    const reason = "unknown persona `admin` — valid: staff, owner, member, operator, partner";
    expect(describeDevLoginFailure(400, undefined, reason)).toBe(reason);
    expect(describeDevLoginFailure(400, undefined)).toContain("`as`");
  });
});

describe("serverReason", () => {
  it("reads a string `error` field and nothing else", () => {
    expect(serverReason({ error: "nope" })).toBe("nope");
    expect(serverReason({ error: 42 })).toBeUndefined();
    expect(serverReason("")).toBeUndefined();
    expect(serverReason(null)).toBeUndefined();
    expect(serverReason(undefined)).toBeUndefined();
  });
});
