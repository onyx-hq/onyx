import { Check, LogOut, Plus, Settings, Shield } from "lucide-react";
import { useNavigate } from "react-router-dom";
import { UserAvatar } from "@/components/UserAvatar";
import { Button } from "@/components/ui/shadcn/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger
} from "@/components/ui/shadcn/dropdown-menu";
import { useAuth } from "@/contexts/AuthContext";
import { useOrgs } from "@/hooks/api/organizations";
import useCurrentUser from "@/hooks/api/users/useCurrentUser";
import { cn } from "@/libs/shadcn/utils";
import ROUTES from "@/libs/utils/routes";
import useCurrentOrg from "@/stores/useCurrentOrg";
import useSettingsDialog from "@/stores/useSettingsDialog";

/** Org switcher section of the user menu (cloud mode only — mounted
 *  conditionally so `useOrgs` never fires in local mode). Ported from the
 *  removed TopBar. */
function OrgSwitcherGroup() {
  const navigate = useNavigate();
  const currentOrg = useCurrentOrg((s) => s.org);
  const { data: orgs } = useOrgs();
  const { data: profile } = useCurrentUser();
  // Orgs are provisioned, not self-served: only staff holding `create_orgs`
  // (what `POST /admin/orgs` requires) get the entry, and it opens the admin
  // tenant directory where that dialog lives.
  const canCreateOrgs = !!profile?.platform_capabilities?.includes("create_orgs");

  return (
    <>
      <DropdownMenuGroup className='flex flex-col gap-1'>
        {orgs?.map((org) => (
          <DropdownMenuItem
            key={org.id}
            onSelect={(e) => {
              // preventDefault skips Radix's auto-close animation; the
              // navigate below unmounts the tree cleanly so no body
              // pointer-events lock leaks onto the destination page.
              e.preventDefault();
              navigate(ROUTES.ORG(org.slug).ROOT);
            }}
            className={cn(
              "flex cursor-pointer items-center gap-2",
              currentOrg?.id === org.id && "bg-muted"
            )}
          >
            <div className='flex h-6 w-6 items-center justify-center rounded bg-primary/10 font-bold text-primary text-xs'>
              {org.name[0]?.toUpperCase()}
            </div>
            <span className='flex-1 truncate'>{org.name}</span>
            {currentOrg?.id === org.id && <Check className='h-4 w-4 text-primary' />}
          </DropdownMenuItem>
        ))}
        {canCreateOrgs && (
          <DropdownMenuItem
            className='cursor-pointer'
            onSelect={(e) => {
              // preventDefault skips Radix's auto-close so the menu unmounts
              // via the navigate instead — no leaking body pointer-events
              // lock on the destination page.
              e.preventDefault();
              navigate(`${ROUTES.ADMIN.TENANTS}?type=orgs`);
            }}
          >
            <Plus className='h-4 w-4' />
            New organization
          </DropdownMenuItem>
        )}
      </DropdownMenuGroup>
      <DropdownMenuSeparator />
    </>
  );
}

/** Rail-bottom user menu: org switcher (cloud), Settings, Admin, Log out. */
export function RailUserMenu() {
  const navigate = useNavigate();
  const { isLocalMode, logout } = useAuth();
  const { data: profile } = useCurrentUser();
  const isAdmin = !!(profile?.is_owner || profile?.is_app_admin);
  const openSettings = useSettingsDialog((s) => s.open);

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          variant='ghost'
          size='icon'
          className='h-8 w-8 rounded-full'
          data-testid='rail-user-menu'
          aria-label='User menu'
        >
          <UserAvatar
            name={profile?.name ?? ""}
            email={profile?.email ?? ""}
            picture={profile?.picture}
            className='h-7 w-7 rounded-full'
          />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent side='right' align='end' className='min-w-48'>
        {!isLocalMode && <OrgSwitcherGroup />}
        <DropdownMenuItem
          className='cursor-pointer'
          onSelect={() =>
            openSettings(isLocalMode ? "workspace.databases" : "organization.general")
          }
        >
          <Settings className='h-4 w-4' />
          Settings
        </DropdownMenuItem>
        {isAdmin && (
          <DropdownMenuItem
            className='cursor-pointer'
            onSelect={() => navigate(ROUTES.ADMIN.CUSTOMER_APPS)}
          >
            <Shield className='h-4 w-4' />
            Admin
          </DropdownMenuItem>
        )}
        {!isLocalMode && (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              className='cursor-pointer text-destructive focus:text-destructive'
              onClick={logout}
            >
              <LogOut className='h-4 w-4' />
              Log out
            </DropdownMenuItem>
          </>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
