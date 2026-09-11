/**
 * Remembers the workspace each user last opened per org, plus the last org they
 * visited. Used by the post-login dispatcher to skip the workspace picker when
 * a returning user already has a clear "resume where I left off" target.
 *
 * Scoped to localStorage because the value is per-device and survives token
 * refreshes; there is no backend column for it. Keys are also scoped by user
 * id so two users sharing the same browser don't inherit each other's "last
 * org / last workspace" preferences — reading `auth_token` alone wouldn't be
 * enough because both users are logged-out-then-in on the same device.
 *
 * When no user id can be resolved (e.g. pre-login), helpers return null on
 * reads and no-op on writes rather than falling back to a global key; there
 * is no legitimate caller in that state.
 */

import type { WorkspaceSummary } from "@/services/api/workspaces";

const LAST_WORKSPACE_PREFIX = "oxy:last_workspace:";
const LAST_ORG_SLUG_PREFIX = "oxy:last_org_slug:";

function currentUserId(): string | null {
  try {
    const raw = localStorage.getItem("user");
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    return typeof parsed?.id === "string" && parsed.id ? parsed.id : null;
  } catch {
    return null;
  }
}

function getLastWorkspaceId(orgId: string): string | null {
  if (!orgId) return null;
  const userId = currentUserId();
  if (!userId) return null;
  try {
    return localStorage.getItem(`${LAST_WORKSPACE_PREFIX}${userId}:${orgId}`);
  } catch {
    return null;
  }
}

export function setLastWorkspaceId(orgId: string, workspaceId: string): void {
  if (!orgId || !workspaceId) return;
  const userId = currentUserId();
  if (!userId) return;
  try {
    localStorage.setItem(`${LAST_WORKSPACE_PREFIX}${userId}:${orgId}`, workspaceId);
  } catch {
    // Storage may be full or disabled — failing silently is acceptable here.
  }
}

export function clearLastWorkspaceId(orgId: string): void {
  if (!orgId) return;
  const userId = currentUserId();
  if (!userId) return;
  try {
    localStorage.removeItem(`${LAST_WORKSPACE_PREFIX}${userId}:${orgId}`);
  } catch {
    // ignore
  }
}

export function getLastOrgSlug(): string | null {
  const userId = currentUserId();
  if (!userId) return null;
  try {
    return localStorage.getItem(`${LAST_ORG_SLUG_PREFIX}${userId}`);
  } catch {
    return null;
  }
}

export function setLastOrgSlug(slug: string): void {
  if (!slug) return;
  const userId = currentUserId();
  if (!userId) return;
  try {
    localStorage.setItem(`${LAST_ORG_SLUG_PREFIX}${userId}`, slug);
  } catch {
    // ignore
  }
}

export function clearLastOrgSlug(): void {
  const userId = currentUserId();
  if (!userId) return;
  try {
    localStorage.removeItem(`${LAST_ORG_SLUG_PREFIX}${userId}`);
  } catch {
    // ignore
  }
}

/**
 * Picks a workspace for a given org, preferring the "last opened" id when it
 * still matches a navigable workspace. Navigable = `ready` or `failed`:
 * cloning is skipped because it's transient (would loop the dispatcher), but
 * failed is kept so the user lands on the last workspace and can retry from
 * the workspace shell instead of being silently routed away. Returns null
 * when no navigable workspace exists — the org root then shows "being set up".
 */
export function pickWorkspace(
  workspaces: WorkspaceSummary[],
  orgId: string
): WorkspaceSummary | null {
  const navigable = workspaces.filter((w) => w.status === "ready" || w.status === "failed");
  if (navigable.length === 0) return null;

  const lastId = getLastWorkspaceId(orgId);
  if (lastId) {
    const byLastId = navigable.find((w) => w.id === lastId);
    if (byLastId) return byLastId;
  }

  const sorted = [...navigable].sort((a, b) => {
    const aTime = a.last_opened_at ? Date.parse(a.last_opened_at) : 0;
    const bTime = b.last_opened_at ? Date.parse(b.last_opened_at) : 0;
    return bTime - aTime;
  });

  return sorted[0];
}
