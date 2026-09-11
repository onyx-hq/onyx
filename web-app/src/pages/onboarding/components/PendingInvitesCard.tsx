import { useQueryClient } from "@tanstack/react-query";
import { isAxiosError } from "axios";
import { Building2, Loader2, Mail, ShieldCheck } from "lucide-react";
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";
import { Button } from "@/components/ui/shadcn/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle
} from "@/components/ui/shadcn/card";
import { useAcceptInvitation } from "@/hooks/api/organizations";
import queryKeys from "@/hooks/api/queryKey";
import ROUTES from "@/libs/utils/routes";
import type { MyInvitation } from "@/types/organization";

/** "You've been invited" card shown when a user with zero orgs lands on
 *  /onboarding and the backend reports pending invitations addressed to their
 *  email. Accepting refreshes the orgs list and drops the user at the org
 *  root, which routes them into a workspace (or "being set up"). */
export default function PendingInvitesCard({ invites }: { invites: MyInvitation[] }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const acceptInvitation = useAcceptInvitation();
  const [acceptingToken, setAcceptingToken] = useState<string | null>(null);

  const handleAccept = async (invite: MyInvitation) => {
    setAcceptingToken(invite.token);
    try {
      const org = await acceptInvitation.mutateAsync(invite.token);
      await queryClient.refetchQueries({ queryKey: queryKeys.org.list() });
      toast.success(`Joined ${org.name}`);
      navigate(ROUTES.ORG(org.slug).ROOT, { replace: true });
    } catch (err) {
      const message = isAxiosError(err)
        ? (err.response?.data?.message ?? "Invitation is no longer valid")
        : "Failed to accept invitation";
      toast.error(message);
      setAcceptingToken(null);
    }
  };

  const plural = invites.length === 1 ? "invitation" : "invitations";

  return (
    <Card className='w-full'>
      <CardHeader className='text-center'>
        <div className='mx-auto mb-2 flex size-10 items-center justify-center rounded-full bg-primary/10'>
          <Mail className='size-5 text-primary' />
        </div>
        <CardTitle className='text-xl'>You've got {plural}</CardTitle>
        <CardDescription>
          {invites.length === 1
            ? "An organization is waiting for you. Accept to jump in."
            : `${invites.length} organizations are waiting for you. Accept one to get started.`}
        </CardDescription>
      </CardHeader>
      <CardContent className='flex flex-col gap-3'>
        <ul className='flex flex-col gap-2'>
          {invites.map((invite) => (
            <li key={invite.id}>
              <div className='flex items-center gap-3 rounded-lg border border-border p-3'>
                <div className='flex size-9 shrink-0 items-center justify-center rounded-md bg-muted'>
                  <Building2 className='size-4 text-muted-foreground' />
                </div>
                <div className='flex min-w-0 flex-1 flex-col'>
                  <span className='truncate font-medium text-sm'>{invite.org_name}</span>
                  <span className='flex items-center gap-2 text-muted-foreground text-xs'>
                    <ShieldCheck className='size-3' />
                    <span className='capitalize'>{invite.role}</span>
                    {invite.invited_by_name && (
                      <>
                        <span>·</span>
                        <span className='truncate'>from {invite.invited_by_name}</span>
                      </>
                    )}
                  </span>
                </div>
                <Button
                  size='sm'
                  onClick={() => handleAccept(invite)}
                  disabled={acceptingToken !== null}
                  data-testid={`onboarding-accept-invite-${invite.org_slug}`}
                >
                  {acceptingToken === invite.token ? (
                    <Loader2 className='size-3.5 animate-spin' />
                  ) : (
                    "Accept"
                  )}
                </Button>
              </div>
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  );
}
