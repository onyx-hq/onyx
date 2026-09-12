import { describe, expect, it } from "vitest";
import type { KioskDeviceRow } from "@/types/frontline";
import { awaitingTablet, kioskState } from "./frontline";

const NOW = Date.parse("2026-09-07T12:00:00Z");
const HOUR = 60 * 60 * 1000;

const kiosk = (over: Partial<KioskDeviceRow> = {}): KioskDeviceRow => ({
  id: "k1",
  name: "Front counter",
  return_to: null,
  created_at: new Date(NOW - HOUR).toISOString(),
  bound_at: null,
  last_seen_at: null,
  revoked_at: null,
  enrol_expires_at: new Date(NOW + 23 * HOUR).toISOString(),
  location_id: null,
  location_name: null,
  idle_timeout_seconds: 300,
  ...over
});

describe("kioskState", () => {
  it("is waiting while the enrol link is live and no tablet has used it", () => {
    expect(kioskState(kiosk(), NOW)).toBe("waiting");
  });

  it("is bound once a tablet holds the cookie, even after the link would have expired", () => {
    const bound = kiosk({
      bound_at: new Date(NOW - HOUR).toISOString(),
      enrol_expires_at: new Date(NOW - 10 * HOUR).toISOString()
    });
    expect(kioskState(bound, NOW)).toBe("bound");
  });

  it("is expired when the link lapsed unused", () => {
    expect(kioskState(kiosk({ enrol_expires_at: new Date(NOW - 1).toISOString() }), NOW)).toBe(
      "expired"
    );
    expect(kioskState(kiosk({ enrol_expires_at: null }), NOW)).toBe("expired");
  });

  it("is revoked whatever else happened — revoked beats bound", () => {
    const revoked = kiosk({
      bound_at: new Date(NOW - HOUR).toISOString(),
      revoked_at: new Date(NOW).toISOString()
    });
    expect(kioskState(revoked, NOW)).toBe("revoked");
  });
});

describe("awaitingTablet", () => {
  it("is true only while some kiosk is still waiting", () => {
    const bound = kiosk({ id: "k2", bound_at: new Date(NOW).toISOString() });
    const revoked = kiosk({ id: "k3", revoked_at: new Date(NOW).toISOString() });
    const expired = kiosk({ id: "k4", enrol_expires_at: new Date(NOW - 1).toISOString() });
    expect(awaitingTablet([bound, revoked, expired], NOW)).toBe(false);
    expect(awaitingTablet([bound, kiosk()], NOW)).toBe(true);
    expect(awaitingTablet([], NOW)).toBe(false);
  });
});
