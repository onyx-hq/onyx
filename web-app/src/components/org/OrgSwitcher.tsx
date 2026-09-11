import { Building2, Check, ChevronsUpDown, Plus } from "lucide-react";
import { useNavigate } from "react-router-dom";
import { Button } from "@/components/ui/shadcn/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger
} from "@/components/ui/shadcn/dropdown-menu";
import { useOrgs } from "@/hooks/api/organizations";
import useCurrentUser from "@/hooks/api/users/useCurrentUser";
import { cn } from "@/libs/shadcn/utils";
import ROUTES from "@/libs/utils/routes";
import useCurrentOrg from "@/stores/useCurrentOrg";

/**
 * Standalone organization switcher for chrome that isn't the rail — e.g. the
 * onboarding header, where the rail is hidden but a multi-org user still needs
 * to jump between organizations. Lists the caller's orgs; staff who may create
 * orgs also get "New organization", which opens the admin tenant directory
 * (orgs are provisioned there, not self-served).
 *
 * Renders nothing when the caller belongs to no org yet: there's nothing to
 * switch between.
 */
export default function OrgSwitcher() {
  const navigate = useNavigate();
  const currentOrg = useCurrentOrg((s) => s.org);
  const { data: orgs } = useOrgs();
  const { data: user } = useCurrentUser();
  // The server's display flag for `Cap::CreateOrgs` — what `POST /admin/orgs` requires.
  const canCreateOrgs = !!user?.platform_capabilities?.includes("create_orgs");

  if (!orgs || orgs.length === 0) return null;

  const active = currentOrg ?? orgs[0];

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          variant='outline'
          size='sm'
          className='max-w-52 gap-2 font-normal'
          data-testid='onboarding-org-switcher'
        >
          <div className='flex size-5 items-center justify-center rounded bg-primary/10 font-bold text-primary text-xs'>
            {active?.name?.[0]?.toUpperCase() ?? <Building2 className='size-3' />}
          </div>
          <span className='flex-1 truncate text-left'>{active?.name ?? "Select organization"}</span>
          <ChevronsUpDown className='size-3.5 shrink-0 text-muted-foreground' />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align='end' className='min-w-52'>
        {orgs.map((org) => (
          <DropdownMenuItem
            key={org.id}
            onSelect={(e) => {
              // preventDefault skips Radix's close animation so the menu
              // unmounts via the navigate below — no body pointer-events lock
              // leaks onto the destination page.
              e.preventDefault();
              navigate(ROUTES.ORG(org.slug).ROOT);
            }}
            className={cn(
              "flex cursor-pointer items-center gap-2",
              org.id === active?.id && "bg-muted"
            )}
          >
            <div className='flex size-6 items-center justify-center rounded bg-primary/10 font-bold text-primary text-xs'>
              {org.name[0]?.toUpperCase()}
            </div>
            <span className='flex-1 truncate'>{org.name}</span>
            {org.id === active?.id && <Check className='size-4 text-primary' />}
          </DropdownMenuItem>
        ))}
        {canCreateOrgs && (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              className='cursor-pointer'
              onSelect={(e) => {
                e.preventDefault();
                navigate(`${ROUTES.ADMIN.TENANTS}?type=orgs`);
              }}
            >
              <Plus className='size-4' />
              New organization
            </DropdownMenuItem>
          </>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
