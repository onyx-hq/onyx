// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { AuthService } from "@/services/api";
import type { OrgInfo, UserInfo } from "@/types/auth";
import {
  consumeReturnTo,
  handlePostLoginOrgs,
  resolveReturnTo,
  returnToFromUrl,
  stashReturnTo
} from "./postLoginRedirect";

// OAuth login (Google/Okta/GitHub) bounces off-domain and back, so a
// `return_to` must survive the round-trip and be validated before we redirect
// into it. These guard the regression where OAuth dropped `return_to` and sent
// the user to the main product instead of the custom-app subdomain they came
// from.
vi.mock("@/services/api", () => ({
  AuthService: { validateReturnTo: vi.fn() }
}));

const APP_URL = "https://acme--store.customer-apps.example.com/";

describe("postLoginRedirect return_to helpers", () => {
  afterEach(() => {
    sessionStorage.clear();
    vi.clearAllMocks();
  });

  it("stashes a return_to and consumes it exactly once", () => {
    stashReturnTo(APP_URL);
    expect(consumeReturnTo()).toBe(APP_URL);
    // Consumed: a second read is empty (no stale return_to lingers).
    expect(consumeReturnTo()).toBeNull();
  });

  it("clears any prior stash when the current attempt has no return_to", () => {
    // Abandon a login started from a custom app, then start a fresh one with
    // no return_to: the stale destination must not leak into the new attempt.
    stashReturnTo(APP_URL);
    stashReturnTo(undefined);
    expect(consumeReturnTo()).toBeNull();

    stashReturnTo(APP_URL);
    stashReturnTo("");
    expect(consumeReturnTo()).toBeNull();
  });

  it("resolves to the url when the server allows it", async () => {
    vi.mocked(AuthService.validateReturnTo).mockResolvedValue(true);
    await expect(resolveReturnTo(APP_URL)).resolves.toBe(APP_URL);
    expect(AuthService.validateReturnTo).toHaveBeenCalledWith(APP_URL);
  });

  it("resolves to null when the server rejects the url", async () => {
    vi.mocked(AuthService.validateReturnTo).mockResolvedValue(false);
    await expect(resolveReturnTo("https://evil.example.com/")).resolves.toBeNull();
  });

  it("short-circuits to null (no server call) when there is no return_to", async () => {
    await expect(resolveReturnTo(undefined)).resolves.toBeNull();
    await expect(resolveReturnTo(null)).resolves.toBeNull();
    expect(AuthService.validateReturnTo).not.toHaveBeenCalled();
  });

  it("reads the return_to query param from the current login URL", () => {
    window.history.pushState({}, "", "/login?return_to=https%3A%2F%2Fapp.example.com%2Fx");
    expect(returnToFromUrl()).toBe("https://app.example.com/x");

    window.history.pushState({}, "", "/login");
    expect(returnToFromUrl()).toBeNull();
  });
});

describe("handlePostLoginOrgs", () => {
  const user = (standing: Partial<UserInfo> = {}) =>
    ({ is_owner: false, is_app_admin: false, ...standing }) as UserInfo;
  const acme = { id: "org-1", name: "Acme", slug: "acme" } as OrgInfo;

  afterEach(() => sessionStorage.clear());

  it("sends a user with no org to the no-org page", () => {
    expect(handlePostLoginOrgs(user(), [])).toBe("/onboarding");
  });

  it("sends staff standing with no org to the admin console, not an invite wait", () => {
    expect(handlePostLoginOrgs(user({ is_app_admin: true }), [])).toBe("/admin/apps");
  });

  it("sends a member of an org to the dispatcher", () => {
    expect(handlePostLoginOrgs(user({ is_app_admin: true }), [acme])).toBe("/");
  });
});
