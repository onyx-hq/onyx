import { useMemo } from "react";
import { Navigate } from "react-router-dom";
import { Spinner } from "@/components/ui/shadcn/spinner";
import { useOrgs } from "@/hooks/api/organizations";
import useCurrentUser from "@/hooks/api/users/useCurrentUser";
import { useAllWorkspaces } from "@/hooks/api/workspaces/useWorkspaces";
import { getInjectedOrg } from "@/libs/orgSubdomain";
import {
  clearLastWorkspaceId,
  getLastOrgSlug,
  pickWorkspace,
  setLastWorkspaceId
} from "@/libs/utils/lastWorkspace";
import ROUTES from "@/libs/utils/routes";
import type { Organization } from "@/types/organization";

/**
 * Landing component at `/`. Runs the "where should this user go?" routine
 * that cannot fit in the synchronous `handlePostLoginOrgs` because it needs
 * to fetch workspaces.
 *
 *   0 orgs                        → /onboarding ("not part of an org yet"),
 *                                   or the admin console for staff standing
 *   has orgs, 0 workspaces        → /:slug (OrgDispatcher renders "being set up")
 *   has orgs, no navigable ws     → /:slug (every ws is still cloning)
 *   has orgs, ≥1 navigable ws     → /:slug/workspaces/:last-or-first-navigable
 *
 * …**unless the user holds a partner role**, in which case every branch that would
 * have left them without a workspace sends them to `/partners` instead. A partner's
 * job is their clients; they do not necessarily want an Oxy workspace of their own.
 * The partner console links straight to their own org.
 *
 * Note this only redirects the no-workspace branches: a partner who DOES have a
 * working workspace still lands in it, because that is their own product and they
 * asked for it by having one.
 *
 * The no-workspace branches hand off to the org root rather than render the
 * "being set up" screen here: under OrgGuard the org store is primed (the
 * workspace creator's GitHub import reads it) and the billing paywall applies.
 *
 * The chosen org follows (a) last-org-slug from localStorage, else (b) the
 * first org returned by the API. Navigable means `status === "ready"` or
 * `"failed"` — cloning is skipped because it's transient, but failed is kept
 * so the user lands on the actual last workspace and can retry from there
 * instead of being silently routed away.
 */
export default function PostLoginDispatcher() {
  const { data: orgs, isPending: orgsPending, isError: orgsError } = useOrgs();
  const { data: user } = useCurrentUser();
  // `partner_memberships` is already on /user — no extra request.
  const isPartner = (user?.partner_memberships?.length ?? 0) > 0;

  const chosenOrg = useMemo(() => pickOrg(orgs), [orgs]);

  // Pass chosenOrg.id explicitly — the dispatcher runs at `/` before any
  // OrgGuard has primed the store, so `useAllWorkspaces`'s store fallback
  // would otherwise either be empty or carry a value from a prior org.
  const {
    data: workspaces,
    isPending: wsPending,
    isError: wsError
  } = useAllWorkspaces(chosenOrg?.id);

  // On a bare org subdomain (`pokehouse.oxygen-hq.com`) the backend injects
  // the org identity, so we skip the org/workspace picker entirely and go
  // straight to the admin-chosen default project (or the org root, which
  // lets OrgDispatcher pick when no default is set). This branch is below the
  // hook calls so the Rules of Hooks hold.
  const injectedOrg = getInjectedOrg();
  if (injectedOrg) {
    return injectedOrg.defaultProjectId ? (
      <Navigate
        to={ROUTES.ORG(injectedOrg.orgSlug).WORKSPACE(injectedOrg.defaultProjectId).ROOT}
        replace
      />
    ) : (
      <Navigate to={ROUTES.ORG(injectedOrg.orgSlug).ROOT} replace />
    );
  }

  if (orgsPending) return <FullPageSpinner />;

  if (orgsError) {
    return (
      <div className='flex h-full w-full items-center justify-center'>
        <p className='text-destructive text-sm'>Failed to load organizations.</p>
      </div>
    );
  }

  if (!orgs || orgs.length === 0) {
    return <Navigate to={noOrgDestination(isPartner, !!user?.is_app_admin)} replace />;
  }

  if (!chosenOrg) return <FullPageSpinner />;

  if (wsPending) return <FullPageSpinner />;

  if (wsError) {
    // Fail open: send the user to the org root so the org dispatcher can retry.
    return <Navigate to={ROUTES.ORG(chosenOrg.slug).ROOT} replace />;
  }

  if (!workspaces || workspaces.length === 0) {
    return (
      <Navigate to={isPartner ? ROUTES.PARTNERS.ROOT : ROUTES.ORG(chosenOrg.slug).ROOT} replace />
    );
  }

  const target = pickWorkspace(workspaces, chosenOrg.id);
  if (!target) {
    // No workspace is navigable yet (all still cloning) — the org root shows
    // "being set up" until one is. Drop any stale per-org lastWorkspace id so
    // next visit doesn't re-select a workspace that isn't navigable.
    clearLastWorkspaceId(chosenOrg.id);
    return (
      <Navigate to={isPartner ? ROUTES.PARTNERS.ROOT : ROUTES.ORG(chosenOrg.slug).ROOT} replace />
    );
  }
  setLastWorkspaceId(chosenOrg.id, target.id);

  return <Navigate to={ROUTES.ORG(chosenOrg.slug).WORKSPACE(target.id).ROOT} replace />;
}

function FullPageSpinner() {
  return (
    <div className='flex h-full w-full items-center justify-center'>
      <Spinner className='size-6' />
    </div>
  );
}

/** Where a user with no org membership lands: their clients, their console, or
 *  the "not part of an org yet" page. Mirrors `handlePostLoginOrgs`. */
function noOrgDestination(isPartner: boolean, hasStaffStanding: boolean): string {
  if (isPartner) return ROUTES.PARTNERS.ROOT;
  if (hasStaffStanding) return ROUTES.ADMIN.CUSTOMER_APPS;
  return ROUTES.ONBOARDING;
}

function pickOrg(orgs: Organization[] | undefined): Organization | null {
  if (!orgs || orgs.length === 0) return null;

  const lastSlug = getLastOrgSlug();
  if (lastSlug) {
    const byLastSlug = orgs.find((o) => o.slug === lastSlug);
    if (byLastSlug) return byLastSlug;
  }

  return orgs[0];
}
