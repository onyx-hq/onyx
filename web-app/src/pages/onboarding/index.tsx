import { useQueryClient } from "@tanstack/react-query";
import { LogOut, MailPlus } from "lucide-react";
import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import JoinOrgDialog from "@/components/org/JoinOrgDialog";
import { Button } from "@/components/ui/shadcn/button";
import { Spinner } from "@/components/ui/shadcn/spinner";
import { useAuth } from "@/contexts/AuthContext";
import { useMyInvitations, useOrgs } from "@/hooks/api/organizations";
import queryKeys from "@/hooks/api/queryKey";
import useCurrentUser from "@/hooks/api/users/useCurrentUser";
import { releaseBodyPointerLock } from "@/libs/utils/pointerEvents";
import ROUTES from "@/libs/utils/routes";
import type { Organization } from "@/types/organization";
import OnboardingHeader from "./components/OnboardingHeader";
import PendingInvitesCard from "./components/PendingInvitesCard";

/**
 * Post-login landing for a user who belongs to no organization. Oxygen orgs are
 * provisioned by the Oxygen team or a partner and people join them by
 * invitation, so there is nothing to create here — only a way in:
 *
 *   pending invite(s) → accept one (the dispatcher then routes into the org)
 *   an invitation link → paste it into the join dialog
 *   wrong account      → log out
 *
 * Accepting refreshes the orgs cache before navigating, so the org root can
 * resolve the joined org immediately.
 */
export default function OnboardingPage() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { logout } = useAuth();
  const { data: user } = useCurrentUser();
  const { data: orgs } = useOrgs();
  const [joinOpen, setJoinOpen] = useState(false);

  const { data: invites, isPending: invitesPending } = useMyInvitations();

  // Clear any body pointer-events lock leaked by a dialog on the page we
  // arrived from — otherwise this page mounts unclickable.
  useEffect(() => {
    releaseBodyPointerLock();
  }, []);

  const handleJoined = async (org: Organization) => {
    setJoinOpen(false);
    await queryClient.refetchQueries({ queryKey: queryKeys.org.list() });
    requestAnimationFrame(() => {
      releaseBodyPointerLock();
      navigate(ROUTES.ORG(org.slug).ROOT, { replace: true });
    });
  };

  if (invitesPending) {
    return (
      <div className='flex min-h-screen min-w-screen items-center justify-center bg-background'>
        <Spinner className='size-6' />
      </div>
    );
  }

  const hasInvites = !!invites && invites.length > 0;
  // Reachable by URL with orgs too; don't tell that user they have none.
  const hasOrgs = (orgs?.length ?? 0) > 0;

  return (
    <div
      className='flex min-h-screen w-full flex-col bg-background'
      data-testid='onboarding-no-org'
    >
      <OnboardingHeader />

      <div className='mx-auto flex w-full max-w-xl flex-1 flex-col items-center justify-center gap-6 px-6 pb-16'>
        {hasInvites ? (
          <PendingInvitesCard invites={invites} />
        ) : (
          <div className='flex flex-col items-center gap-2 text-center'>
            <h1 className='font-semibold text-2xl tracking-tight'>
              {hasOrgs ? "Join an organization" : "You're not part of an organization yet"}
            </h1>
            <p className='max-w-md text-muted-foreground text-sm'>
              People join an Oxygen organization by invitation. Ask your admin to invite{" "}
              {user?.email ? (
                <span className='font-medium text-foreground'>{user.email}</span>
              ) : (
                "you"
              )}
              , or paste the invitation link you were sent.
            </p>
          </div>
        )}

        <div className='flex flex-wrap items-center justify-center gap-2'>
          <Button
            variant={hasInvites ? "ghost" : "default"}
            onClick={() => setJoinOpen(true)}
            data-testid='onboarding-join-org-button'
          >
            <MailPlus className='size-4' />
            Join with an invitation link
          </Button>
          <Button
            variant='ghost'
            className='text-muted-foreground'
            onClick={logout}
            data-testid='onboarding-log-out-button'
          >
            <LogOut className='size-4' />
            Log out
          </Button>
        </div>
      </div>

      <JoinOrgDialog open={joinOpen} onOpenChange={setJoinOpen} onJoined={handleJoined} />
    </div>
  );
}
