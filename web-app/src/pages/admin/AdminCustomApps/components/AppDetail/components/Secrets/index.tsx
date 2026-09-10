import { Plus, TriangleAlert } from "lucide-react";
import { useState } from "react";
import { Button } from "@/components/ui/shadcn/button";
import { useAppSecrets } from "@/hooks/api/customApps/useAppSecrets";
import { SecretRow } from "./SecretRow";
import { SetSecretDialog } from "./SetSecretDialog";

/**
 * The AppDetail "Secrets" section: every key this app needs, and whether it has
 * one.
 *
 * The list is the union of two things, which is the whole point of the section.
 * What the app's active build **declares** — the `env` block in `oxy-app.json`,
 * plus every function's `webhook.secretVar` — and what is actually **stored**
 * under `apps/<app_id>/`. Before this, the only way to see an app secret was the
 * workspace settings table, where it appeared as a raw `apps/<uuid>/KEY` row
 * among every other project secret, and the only way to *create* one was from
 * inside a function that could not run without it.
 *
 * Rows a person has to act on come first and are the only coloured thing here.
 * A healthy key carries no badge at all: badging every row is what stops the
 * broken one from standing out.
 */
export const Secrets = ({ appId }: { appId: string }) => {
  const { data, isLoading, error } = useAppSecrets(appId);
  /** The key being set. `""` = a new one; a name = rotating that one. */
  const [editing, setEditing] = useState<string | null>(null);

  if (isLoading) {
    return <p className='text-muted-foreground text-xs'>Loading secrets…</p>;
  }
  if (error || !data) {
    return <p className='text-destructive text-xs'>Couldn't load secrets.</p>;
  }

  return (
    <div className='flex flex-col gap-2' data-testid='admin-app-secrets'>
      {data.declaration_error && <DeclarationError message={data.declaration_error} />}

      {data.entries.length === 0 ? (
        <EmptyState />
      ) : (
        <ul className='flex flex-col gap-1.5' data-testid='admin-app-secrets-list'>
          {data.entries.map((entry) => (
            <SecretRow key={entry.key} appId={appId} entry={entry} onSet={setEditing} />
          ))}
        </ul>
      )}

      <div>
        <Button
          type='button'
          variant='outline'
          size='sm'
          className='h-7 gap-1.5 text-xs'
          onClick={() => setEditing("")}
          data-testid='admin-app-secrets-add'
        >
          <Plus className='size-3' />
          Add secret
        </Button>
      </div>

      <SetSecretDialog
        appId={appId}
        // A named key is a rotation, so the field is locked; `""` is a new one.
        secretKey={editing}
        onClose={() => setEditing(null)}
      />
    </div>
  );
};

/**
 * The app has neither declarations nor stored keys. An empty screen is an
 * invitation to act, so it names both ways forward — declare what the app needs
 * so the next person sees the list, or just set one now.
 */
const EmptyState = () => (
  <p className='text-muted-foreground text-xs' data-testid='admin-app-secrets-empty'>
    No secrets yet. Add an <code>env</code> block to <code>oxy-app.json</code> to list the keys this
    app expects, or set one below. Functions read them as <code>ctx.env.&lt;KEY&gt;</code>.
  </p>
);

/**
 * The manifest declares an `env` block that would not parse. Reading it
 * leniently keeps the app working, but staying quiet about it would make a typo
 * look exactly like an app that declares nothing — so it is said out loud, here,
 * next to the list it emptied.
 */
const DeclarationError = ({ message }: { message: string }) => (
  <div
    className='flex items-start gap-1.5 rounded-md border border-warning/40 bg-warning/10 px-2.5 py-2 text-xs'
    data-testid='admin-app-secrets-declaration-error'
  >
    <TriangleAlert className='mt-px size-3 shrink-0 text-warning' />
    <span className='text-muted-foreground'>{message}</span>
  </div>
);

/**
 * The count in the section header, so a collapsed section still says the app is
 * missing something. Reads the same query as the pane — react-query dedupes, so
 * this costs no extra request — rather than threading the number down through
 * the dossier, which does not otherwise know about any section's data.
 *
 * Renders nothing when there is nothing wrong: a "0 missing" badge is noise on
 * every healthy app, and it would sit beside seven other sections that show no
 * badge at all.
 */
export const SecretsBadge = ({ appId }: { appId: string }) => {
  const { data } = useAppSecrets(appId);
  if (!data?.missing_required) return null;
  return (
    <span
      className='rounded-sm bg-destructive/15 px-1.5 py-px font-medium text-[10px] text-destructive tabular-nums tracking-normal'
      data-testid='admin-app-secrets-missing-badge'
    >
      {data.missing_required} missing
    </span>
  );
};
