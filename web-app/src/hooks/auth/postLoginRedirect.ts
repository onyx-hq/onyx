import ROUTES from "@/libs/utils/routes";
import { AuthService } from "@/services/api";
import type { OrgInfo, UserInfo } from "@/types/auth";

export const PENDING_INVITE_TOKEN_KEY = "pending_invite_token";

const RETURN_TO_KEY = "oxy_post_login_return_to";

/**
 * Persist a `return_to` across an OAuth provider round-trip.
 *
 * OAuth bounces the browser off-domain (to Google/Okta/GitHub) and back to a
 * fixed callback URL, so a `return_to` query param can't ride along, and the
 * signed `state` token is reserved for CSRF. We stash it in `sessionStorage`
 * (same mechanism the CSRF state uses, and it survives the same-origin return)
 * and read it back in the callback.
 *
 * Always reflects the CURRENT attempt: an empty value clears any previously
 * stashed destination, so abandoning a login started from a custom app and
 * later signing in from a plain `/login` doesn't redirect into the stale app.
 */
export function stashReturnTo(returnTo: string | null | undefined): void {
  if (returnTo) {
    sessionStorage.setItem(RETURN_TO_KEY, returnTo);
  } else {
    sessionStorage.removeItem(RETURN_TO_KEY);
  }
}

/** Read and clear the stashed `return_to` (see {@link stashReturnTo}). */
export function consumeReturnTo(): string | null {
  const value = sessionStorage.getItem(RETURN_TO_KEY);
  if (value) {
    sessionStorage.removeItem(RETURN_TO_KEY);
  }
  return value;
}

/**
 * Resolve a post-login `return_to` into a safe destination. Returns the URL
 * only when the server confirms it's allowed (see `validateReturnTo`), so the
 * caller can `window.location.href` into it (e.g. back to a custom-app
 * subdomain). Returns `null` when there's no `return_to` or the server rejects
 * it — the caller then falls back to {@link handlePostLoginOrgs}.
 */
export async function resolveReturnTo(returnTo: string | null | undefined): Promise<string | null> {
  if (!returnTo) {
    return null;
  }
  if (await AuthService.validateReturnTo(returnTo)) {
    return returnTo;
  }
  console.warn("return_to URL rejected by server; falling back to default destination");
  return null;
}

/** Read the `return_to` query param from the current login URL, if present. */
export function returnToFromUrl(): string | null {
  return new URLSearchParams(window.location.search).get("return_to");
}

export function handlePostLoginOrgs(user: UserInfo, orgs: OrgInfo[]): string {
  const pendingInviteToken = sessionStorage.getItem(PENDING_INVITE_TOKEN_KEY);
  if (pendingInviteToken) {
    sessionStorage.removeItem(PENDING_INVITE_TOKEN_KEY);
    return ROUTES.INVITE(pendingInviteToken);
  }

  if (user.is_owner) {
    return ROUTES.ADMIN.BILLING_QUEUE;
  }

  if (orgs.length === 0) {
    // Staff standing with no membership (an App Operator, a Global Admin before
    // any tenant exists) is working in the admin console, not waiting on an
    // invite. `/onboarding` stays reachable by URL for a staff invite.
    return user.is_app_admin ? ROUTES.ADMIN.CUSTOMER_APPS : ROUTES.ONBOARDING;
  }

  return ROUTES.ROOT;
}
