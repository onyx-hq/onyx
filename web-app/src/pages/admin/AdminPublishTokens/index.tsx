import { KeyRound, Loader2, Plus } from "lucide-react";
import { type FormEvent, useState } from "react";
import { Button } from "@/components/ui/shadcn/button";
import { Card, CardContent } from "@/components/ui/shadcn/card";
import { Input } from "@/components/ui/shadcn/input";
import { Spinner } from "@/components/ui/shadcn/spinner";
import {
  useCreatePublishToken,
  usePublishTokens,
  useRevokePublishToken
} from "@/hooks/api/publishTokens/usePublishTokens";
import type { CreatedPublishToken, PublishToken } from "@/types/publishTokens";
import CiInstructions from "./components/CiInstructions";
import { CreatedTokenDialog } from "./components/CreatedTokenDialog";
import { PublishTokenRow } from "./components/PublishTokenRow";
import { RevokeTokenDialog } from "./components/RevokeTokenDialog";

/**
 * `/admin/publish-tokens` — manage **App publish tokens**: long-lived
 * bearer credentials for machine auth (primarily `oxyc publish` in CI),
 * a stable replacement for the ~7-day session JWT.
 *
 * Open to any Global Admin (the `app_admins` table). A live token acts as
 * its minting admin **only on the customer-apps publish surface** — it
 * cannot delete apps, mint app API keys, or manage tokens (see the
 * `app_publish_token_scope` middleware). Tokens are managed across admins:
 * anyone here can revoke anyone's token.
 */
export default function AdminPublishTokens({ embedded = false }: { embedded?: boolean } = {}) {
  const { data: tokens = [], isPending } = usePublishTokens();
  const create = useCreatePublishToken();
  const revoke = useRevokePublishToken();
  const [name, setName] = useState("");
  const [created, setCreated] = useState<CreatedPublishToken | null>(null);
  const [pendingRevoke, setPendingRevoke] = useState<PublishToken | null>(null);

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const trimmed = name.trim();
    create.mutate(trimmed, {
      onSuccess: (token) => {
        setCreated(token);
        setName("");
      }
    });
  };

  const confirmRevoke = () => {
    if (!pendingRevoke) return;
    revoke.mutate(pendingRevoke.id, {
      onSettled: () => setPendingRevoke(null)
    });
  };

  return (
    <div className='mx-auto max-w-3xl p-6'>
      <div className='mb-6'>
        {!embedded && <h1 className='font-semibold text-xl tracking-tight'>Publish tokens</h1>}
        <p
          className={
            embedded ? "text-muted-foreground text-xs" : "mt-1 text-muted-foreground text-xs"
          }
        >
          Long-lived bearer tokens for machine auth — set one as the{" "}
          <span className='font-mono'>OXY_TOKEN</span> secret so{" "}
          <span className='font-mono'>oxyc publish</span> works in CI without an expiring login. A
          token can publish and read the custom-apps surface only; it can't delete apps, mint app
          API keys, or manage tokens.
        </p>
      </div>

      <Card className='mb-6'>
        <CardContent className='p-4'>
          <form onSubmit={onSubmit} className='flex flex-col gap-3 sm:flex-row sm:items-center'>
            <div className='relative flex-1'>
              <Input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder='Token name (e.g. ci-publish)'
                disabled={create.isPending}
                autoComplete='off'
              />
            </div>
            <Button type='submit' disabled={create.isPending || !name.trim()}>
              {create.isPending ? (
                <>
                  <Loader2 className='size-4 animate-spin' />
                  Creating…
                </>
              ) : (
                <>
                  <Plus className='size-4' />
                  Create token
                </>
              )}
            </Button>
          </form>
        </CardContent>
      </Card>

      <CiInstructions />

      <Card>
        <CardContent className='p-0'>
          {isPending ? (
            <div className='flex items-center justify-center gap-2 py-16 text-muted-foreground text-xs'>
              <Spinner /> Loading…
            </div>
          ) : tokens.length === 0 ? (
            <div className='flex flex-col items-center justify-center gap-2 py-16 text-muted-foreground'>
              <KeyRound className='size-8' />
              <p className='text-xs'>No publish tokens yet.</p>
              <p className='text-xs'>Create one above to authenticate `oxyc publish` from CI.</p>
            </div>
          ) : (
            <ul className='divide-y divide-border'>
              {tokens.map((token) => (
                <PublishTokenRow key={token.id} token={token} onRevoke={setPendingRevoke} />
              ))}
            </ul>
          )}
        </CardContent>
      </Card>

      <CreatedTokenDialog token={created} onClose={() => setCreated(null)} />

      <RevokeTokenDialog
        token={pendingRevoke}
        isRevoking={revoke.isPending}
        onOpenChange={(open) => {
          if (!open && !revoke.isPending) setPendingRevoke(null);
        }}
        onConfirm={confirmRevoke}
      />
    </div>
  );
}
