/**
 * What to tell a developer when `/dev-login` is refused. The bare refusals
 * (404 / 403 / 401) are the endpoint's normal vocabulary, and each has exactly
 * one fix, so the copy names it rather than saying "something went wrong".
 * 400 and 409 (a bad or unseeded `?as=` persona) carry the server's own reason,
 * which names the valid personas or the seed command — shown verbatim.
 *
 * Lives beside the page rather than inside it so the copy can be pinned by a
 * test without dragging the axios client, AuthContext and the shadcn tree into
 * a suite that only needs a pure string function.
 */
export const describeDevLoginFailure = (
  httpStatus: number | undefined,
  email: string | undefined,
  reason?: string
): string => {
  switch (httpStatus) {
    case 404:
      // Two ways to land here, and the explicit var fixes both — so it leads.
      // The fallback caveat is second because the reader most likely to see a
      // genuine "not enabled" is on a release binary (`oxy start`, a Docker
      // image), where the debug-only fallback is inert.
      return "Dev sign-in is not enabled for you on this server. Set OXY_DEV_LOGIN_EMAILS and restart it. (On a debug build, an unset value falls back to OXY_GLOBAL_ADMINS — but only for requests from the server's own machine, so browsing it by LAN address lands here too.)";
    case 403:
      return email
        ? `"${email}" is not listed in OXY_DEV_LOGIN_EMAILS / OXY_GLOBAL_ADMINS on this server.`
        : "That identity is not listed in OXY_DEV_LOGIN_EMAILS / OXY_GLOBAL_ADMINS on this server.";
    case 401:
      return "That user is marked deleted, so the server refused to restore it.";
    case 400:
      return reason ?? "The server rejected the sign-in request — pass `email` or `as`, not both.";
    case 409:
      return reason ?? "That persona is not seeded on this server — run `just up` (or `oxy seed`).";
    default:
      return "Dev sign-in failed. Check the server logs.";
  }
};

/** The `{"error": "..."}` reason a 400 / 409 refusal carries, if any. */
export const serverReason = (data: unknown): string | undefined => {
  if (typeof data !== "object" || data === null || !("error" in data)) {
    return undefined;
  }
  return typeof data.error === "string" ? data.error : undefined;
};
