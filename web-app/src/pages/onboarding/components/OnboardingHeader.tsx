import { ChevronLeft, LogOut } from "lucide-react";
import { useNavigate } from "react-router-dom";
import OrgSwitcher from "@/components/org/OrgSwitcher";
import { Button } from "@/components/ui/shadcn/button";
import { useAuth } from "@/contexts/AuthContext";
import { useOrgs } from "@/hooks/api/organizations";
import useCurrentUser from "@/hooks/api/users/useCurrentUser";
import ROUTES from "@/libs/utils/routes";
import useCurrentOrg from "@/stores/useCurrentOrg";
import useTheme from "@/stores/useTheme";

export default function OnboardingHeader() {
  const navigate = useNavigate();
  const { theme } = useTheme();
  const { logout } = useAuth();
  const { data: currentUser } = useCurrentUser();
  const { data: orgs } = useOrgs();
  const previousOrgSlug = useCurrentOrg((s) => s.org?.slug);
  const hasAnyWorkspace = !!orgs?.some((o) => (o.workspace_count ?? 0) > 0);

  // Prefer the org the user was on before landing here so "Back" returns them
  // there. Falls back to root, which lets PostLoginDispatcher pick a workspace
  // when no previous org is tracked.
  const handleBack = () => {
    if (previousOrgSlug) {
      navigate(ROUTES.ORG(previousOrgSlug).ROOT);
    } else {
      navigate(ROUTES.ROOT);
    }
  };

  return (
    <div className='flex items-center justify-between gap-2 p-6 font-medium'>
      {hasAnyWorkspace ? (
        <Button
          variant='ghost'
          size='sm'
          onClick={handleBack}
          className='-ml-2 gap-1 text-muted-foreground hover:text-foreground'
        >
          <ChevronLeft className='size-4' />
          Back to Oxygen
        </Button>
      ) : (
        <div className='flex items-center gap-2'>
          <img src={theme === "dark" ? "/oxygen-dark.svg" : "/oxygen-light.svg"} alt='Oxygen' />
          <span className='truncate text-sm'>Oxygen</span>
        </div>
      )}
      <div className='flex items-center gap-4'>
        {/* The rail (and its switcher) is hidden here, so a user who does
            belong to an org can jump to it from the header. Renders nothing
            when the user has no org yet. */}
        <OrgSwitcher />
        {currentUser?.email && (
          <div className='group relative'>
            <div className='flex cursor-pointer flex-col items-end text-right leading-tight'>
              <span className='text-muted-foreground text-xs'>Logged in as</span>
              <span className='truncate font-normal text-sm'>{currentUser.email}</span>
            </div>
            <div className='pointer-events-none absolute top-full right-0 z-10 pt-2 opacity-0 transition-opacity focus-within:pointer-events-auto focus-within:opacity-100 group-hover:pointer-events-auto group-hover:opacity-100'>
              <Button
                variant='ghost'
                size='sm'
                onClick={logout}
                aria-label='Log out'
                className='gap-1.5 shadow-sm'
              >
                <LogOut className='size-3.5' />
                Log out
              </Button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
