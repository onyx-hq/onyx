import { describe, expect, it } from "vitest";
import { appSecretName, parseAppSecretName } from "./appSecretName";

describe("parseAppSecretName", () => {
  it("splits an app-scoped name into app and key", () => {
    expect(parseAppSecretName("apps/018f-abc/STRIPE_API_KEY")).toEqual({
      appId: "018f-abc",
      key: "STRIPE_API_KEY"
    });
  });

  it("leaves an ordinary project secret alone", () => {
    expect(parseAppSecretName("OPENAI_API_KEY")).toBeNull();
  });

  it("keeps dots and hyphens, which are legal in a key", () => {
    expect(parseAppSecretName("apps/a/my.key-name")?.key).toBe("my.key-name");
  });

  // A name that merely starts with `apps/` is not app-scoped. Treating it as
  // such would hide it under an app that does not exist, and the row would
  // vanish from the table rather than read oddly.
  it("ignores a name with the wrong number of segments", () => {
    expect(parseAppSecretName("apps/only-two")).toBeNull();
    expect(parseAppSecretName("apps/a/b/c")).toBeNull();
    expect(parseAppSecretName("apps//KEY")).toBeNull();
  });

  it("round-trips with appSecretName", () => {
    const name = appSecretName("app-1", "TOKEN");
    expect(name).toBe("apps/app-1/TOKEN");
    expect(parseAppSecretName(name)).toEqual({ appId: "app-1", key: "TOKEN" });
  });
});
