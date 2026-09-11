import { Building2, LogOut, Plus, Settings } from "lucide-react";
import { useEffect } from "react";
import { useNavigate } from "react-router-dom";
import OxyLogo from "@/components/OxyLogo";
import SettingsDialog from "@/components/settings/SettingsDialog";
import { useSettingsDeepLink } from "@/components/settings/SettingsDialog/useSettingsDeepLink";
import { Button } from "@/components/ui/shadcn/button";
import { useAuth } from "@/contexts/AuthContext";
import { useOrgs } from "@/hooks/api/organizations";
import useCurrentUser from "@/hooks/api/users/useCurrentUser";
import { releaseBodyPointerLock } from "@/libs/utils/pointerEvents";
import ROUTES from "@/libs/utils/routes";
import useCurrentWorkspace from "@/stores/useCurrentWorkspace";
import useSettingsDialog from "@/stores/useSettingsDialog";
import type { Organization } from "@/types/organization";
import CreateWorkspacePanel from "./components/CreateWorkspacePanel";

interface Props {
  /** Resolved from the URL slug by the dispatcher, not the zustand store. */
  org: Organization;
  /** The viewer opened "Create workspace". Owned by the dispatcher so a
   *  workspace that turns ready mid-creation doesn't navigate away from the
   *  preparing screen — the creator hands off to the setup wizard itself. */
  creating: boolean;
  onCreatingChange: (creating: boolean) => void;
}

/**
 * What an org with no ready workspace shows at `/:orgSlug`.
 *
 * Orgs are provisioned by the Oxygen team or a partner, and every new org gets
 * a Default workspace — so "nothing ready" means setup is still in flight, not
 * that the viewer should build something. This replaced a redirect into a
 * self-serve onboarding wizard.
 *
 * "Create workspace" opens the existing WorkspaceCreator flow, so the setup
 * wizard stays reachable on purpose for the people whose job it is: staff
 * standing AND an Owner/Admin role in this org. Both halves matter. A customer's
 * own Owner is told the Oxygen team is on it, so offering them the wizard would
 * contradict that; and every endpoint the creator calls takes `OrgAdmin`, which
 * reads the org role only, so staff who are a plain Member here would get a 403.
 * Staff acting through an assume-role session arrive with the synthetic Owner
 * role, which `GET /orgs` reports, so they pass. Org admins also get the
 * organization settings (crew, locations, app access), none of which need a
 * workspace. Both are display gates — the server re-checks every call.
 */
export default function OrgSetupPending({ org, creating, onCreatingChange }: Props) {
  const navigate = useNavigate();
  const { logout } = useAuth();
  const { data: user } = useCurrentUser();
  const { data: orgs } = useOrgs();
  const openSettings = useSettingsDialog((s) => s.open);

  // `/<org>?settings=organization.crew` opens the dialog straight away.
  useSettingsDeepLink();

  // Clear a body pointer lock leaked by a dialog on the page we came from, and
  // retire the last visit's workspace: `useCurrentWorkspace` is only ever
  // written by the workspace layout, so arriving here from another org would
  // hand the settings dialog that org's Workspace group.
  useEffect(() => {
    releaseBodyPointerLock();
    useCurrentWorkspace.getState().setWorkspace(null);
  }, []);

  // Platform standing is the server's display flag; the org role is what
  // `GET /orgs` returned for this membership.
  const isOrgAdmin = org.role === "owner" || org.role === "admin";
  const canCreateWorkspace = !!(user?.is_owner || user?.is_app_admin) && isOrgAdmin;
  const otherOrgs = (orgs ?? []).filter((o) => o.id !== org.id);

  return (
    <div
      className='flex min-h-screen w-full flex-col bg-background'
      data-testid='org-setup-pending'
    >
      <div className='flex items-center justify-between gap-4 p-6'>
        <div className='flex items-center gap-2 font-medium'>
          <OxyLogo />
          <span className='truncate text-sm'>Oxygen</span>
        </div>
        {user?.email && (
          <span className='truncate text-muted-foreground text-sm'>{user.email}</span>
        )}
      </div>

      <div className='flex flex-1 items-center justify-center px-6 pb-16'>
        {creating && canCreateWorkspace ? (
          <CreateWorkspacePanel org={org} onBack={() => onCreatingChange(false)} />
        ) : (
          <div className='w-full max-w-md space-y-6 rounded-2xl border bg-card p-8 text-center'>
            <div className='space-y-2'>
              <div className='mx-auto mb-4 flex size-10 items-center justify-center rounded-full bg-primary/10'>
                <Building2 className='size-5 text-primary' />
              </div>
              <h1
                className='font-serif text-2xl tracking-tight'
                data-testid='org-setup-pending-title'
              >
                {org.name} is being set up
              </h1>
              <p className='text-muted-foreground text-sm'>
                The Oxygen team is setting up your workspace and apps. They'll open here as soon as
                they're ready.
              </p>
            </div>

            {(canCreateWorkspace || isOrgAdmin) && (
              <div className='space-y-2'>
                {canCreateWorkspace && (
                  <Button
                    className='w-full'
                    onClick={() => onCreatingChange(true)}
                    data-testid='org-setup-pending-create-workspace'
                  >
                    <Plus className='size-4' />
                    Create workspace
                  </Button>
                )}
                {isOrgAdmin && (
                  <Button
                    variant='outline'
                    className='w-full'
                    onClick={() => openSettings("organization.general")}
                    data-testid='org-setup-pending-open-settings'
                  >
                    <Settings className='size-4' />
                    Organization settings
                  </Button>
                )}
              </div>
            )}

            {otherOrgs.length > 0 && (
              <div className='space-y-2 text-left'>
                <p className='text-muted-foreground text-xs'>Switch organization</p>
                <div className='space-y-1'>
                  {otherOrgs.map((other) => (
                    <button
                      key={other.id}
                      type='button'
                      onClick={() => navigate(ROUTES.ORG(other.slug).ROOT)}
                      data-testid={`org-setup-pending-switch-${other.slug}`}
                      className='flex w-full items-center justify-between rounded-md border bg-background p-3 text-sm transition-colors hover:bg-accent hover:text-accent-foreground'
                    >
                      <span className='font-medium'>{other.name}</span>
                      <span className='text-muted-foreground text-xs capitalize'>{other.role}</span>
                    </button>
                  ))}
                </div>
              </div>
            )}

            <Button
              variant='ghost'
              className='w-full text-muted-foreground'
              onClick={logout}
              data-testid='org-setup-pending-log-out'
            >
              <LogOut className='size-4' />
              Log out
            </Button>
          </div>
        )}
      </div>

      <SettingsDialog />
    </div>
  );
}
