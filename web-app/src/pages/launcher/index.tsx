import { useEffect, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { AppAccessDialog } from "@/components/appAccess/AppAccessDialog";
import { RecentThreads } from "@/components/RecentThreads";
import { Spinner } from "@/components/ui/shadcn/spinner";
import { useCustomApps } from "@/hooks/api/customApps/useCustomApps";
import useCurrentProjectBranch from "@/hooks/useCurrentProjectBranch";
import useAskDock from "@/stores/useAskDock";
import useCurrentOrg from "@/stores/useCurrentOrg";
import type { CustomAppSummary } from "@/types/apps";
import { AppCard } from "./components/AppCard";
import { AskForwardFallback } from "./components/AskForwardFallback";
import { CriticalAlertBanner } from "./components/CriticalAlertBanner";
import { NeedsAttention } from "./components/NeedsAttention";
import { ProjectSetupToast } from "./components/ProjectSetupToast";
import useWorkspaceReadiness from "./useWorkspaceReadiness";

const LauncherPage = () => {
  const readiness = useWorkspaceReadiness();
  const { project } = useCurrentProjectBranch();
  const { data: customApps = [], isPending: appsPending } = useCustomApps(project?.id ?? "");
  const location = useLocation();
  const navigate = useNavigate();
  const openAsk = useAskDock((s) => s.open);

  // Managing access from the home grid, so an org admin doesn't have to know that
  // the control lives three levels into the settings dialog. One dialog for the
  // whole grid rather than one per card — the cards only report which app.
  //
  // Owner and admin are the roles `Ring::AppGrant` accepts; the server re-checks,
  // so this gate decides whether the affordance is worth showing, not who gets in.
  // An operator acting as a tenant resolves to owner (staff) or admin (a partner
  // acting on a client), so assume-role sessions get the control too.
  //
  // One value rather than a boolean beside the org: it narrows, so the dialog can
  // read `orgId` off it without a second `org &&` guard that could drift out of
  // agreement with the gate.
  const org = useCurrentOrg((s) => s.org);
  const orgRole = useCurrentOrg((s) => s.role);
  const manageableOrgId = org && (orgRole === "owner" || orgRole === "admin") ? org.id : null;
  const [managing, setManaging] = useState<CustomAppSummary | null>(null);

  // Back-compat with navigate(HOME, { state: { prefillQuestion, ... } })
  // callers (onboarding EXPLORE buttons etc.): open the Ask dock
  // prefilled instead of rendering an inline composer.
  const locationState = location.state as {
    prefillQuestion?: string;
    agentPath?: string;
    autoSubmit?: boolean;
  } | null;
  useEffect(() => {
    if (readiness.status !== "ready") return;
    if (!locationState?.prefillQuestion && !locationState?.agentPath) return;
    openAsk({
      message: locationState.prefillQuestion,
      agentPath: locationState.agentPath,
      autoSubmit: locationState.autoSubmit
    });
    // Consume the state so back/forward/refresh doesn't re-open or re-submit.
    navigate(location.pathname + location.search, { replace: true, state: null });
  }, [readiness.status, locationState, openAsk, navigate, location.pathname, location.search]);

  if (readiness.status === "loading" || appsPending) {
    return (
      <div className='flex h-full items-center justify-center'>
        <Spinner className='size-6' />
      </div>
    );
  }

  const hasApps = customApps.length > 0;

  return (
    <div className='flex h-full flex-col overflow-auto'>
      <ProjectSetupToast gaps={readiness.gaps} />
      {hasApps ? (
        <>
          {/* The HQ heading + status line moved to the universal top bar
              (breadcrumb "<Workspace> / HQ"). */}
          {/* Critical alerts only — hidden in the calm default state */}
          <CriticalAlertBanner />
          {/* Apps — the primary operating surfaces, lead the page */}
          <div className='mx-auto w-full max-w-6xl px-6 pt-12 pb-8'>
            <div
              className='grid grid-cols-1 gap-5 sm:grid-cols-2 xl:grid-cols-3'
              data-testid='launcher-app-grid'
            >
              {customApps.map((app, i) => (
                // art_url in the key remounts the card when art changes, resetting its error fallback
                <AppCard
                  key={`${app.id}:${app.art_url ?? ""}`}
                  app={app}
                  // The first row at the widest layout (xl:grid-cols-3) is on screen at
                  // first paint, so its art should not wait for layout; the rest stay lazy.
                  eagerArt={i < 3}
                  onManageAccess={manageableOrgId ? setManaging : undefined}
                />
              ))}
            </div>
          </div>
          {/* Needs attention — secondary intelligence module below the apps */}
          <NeedsAttention />
        </>
      ) : (
        <AskForwardFallback
          shouldDisableChat={readiness.shouldDisableChat}
          hasSetupSteps={readiness.gaps.length > 0}
        />
      )}
      <RecentThreads className='mx-auto w-full max-w-6xl px-6 pb-8' />
      {/* Gated on the same value as the button, so a member doesn't mount a dialog
          they can never open. (This is a render cost only — the dialog's queries
          are already keyed on `open`, so nothing was being fetched.) */}
      {manageableOrgId && (
        <AppAccessDialog
          open={managing !== null}
          onOpenChange={(open) => !open && setManaging(null)}
          scope={{ kind: "org", orgId: manageableOrgId }}
          appId={managing?.id ?? null}
          appName={managing?.name ?? ""}
        />
      )}
    </div>
  );
};

export default LauncherPage;
