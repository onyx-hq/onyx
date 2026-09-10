import { Eye, EyeOff, KeyRound, Trash2, TriangleAlert } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/shadcn/button";
import { useDeleteAppSecret } from "@/hooks/api/customApps/useAppSecrets";
import { cn } from "@/libs/shadcn/utils";
import { relativeTime } from "@/pages/admin/utils";
import { CustomAppsService } from "@/services/api/customApps";
import type { AppSecretEntry } from "@/types/apps";

/**
 * One key. Three states, and only one of them is loud:
 *
 * - **Missing** — the build asks for it and nothing is stored. Carries the row's
 *   only colour and its only filled button, because it is the only row that is
 *   a task rather than a fact.
 * - **Set** — no badge. A tick beside every healthy key would compete with the
 *   one row that needs reading.
 * - **Undeclared** — stored, but nothing in the active build asks for it. A
 *   quiet outline badge, not a warning: a token a function refreshed into place
 *   via `ctx.secrets.set` lives here permanently and is working exactly as
 *   intended.
 */
export const SecretRow = ({
  appId,
  entry,
  onSet
}: {
  appId: string;
  entry: AppSecretEntry;
  /** Opens the set/rotate dialog for this key. */
  onSet: (key: string) => void;
}) => {
  const missing = !entry.is_set;
  const urgent = missing && entry.required;

  return (
    <li
      className={cn(
        "rounded-md border bg-card px-3 py-2",
        urgent ? "border-destructive/40" : "border-border"
      )}
      data-testid={`admin-app-secret-${entry.key}`}
    >
      <div className='flex items-center gap-2'>
        {urgent ? (
          <TriangleAlert className='size-3.5 shrink-0 text-destructive' />
        ) : (
          <KeyRound
            className={cn(
              "size-3.5 shrink-0",
              missing ? "text-muted-foreground" : "text-vis-violet"
            )}
          />
        )}
        <span
          className={cn(
            "truncate font-mono text-xs",
            missing && !urgent && "text-muted-foreground"
          )}
        >
          {entry.key}
        </span>

        {urgent && <StateBadge tone='destructive'>Missing</StateBadge>}
        {missing && !urgent && <StateBadge tone='muted'>Not set</StateBadge>}
        {entry.is_set && !entry.declared && <StateBadge tone='outline'>Undeclared</StateBadge>}

        <div className='ml-auto flex shrink-0 items-center gap-1'>
          {entry.is_set && (
            <span className='mr-1 text-[10px] text-muted-foreground'>
              {relativeTime(entry.updated_at)}
              {entry.updated_by_email ? ` · ${entry.updated_by_email}` : ""}
            </span>
          )}
          {entry.is_set && <RevealButton appId={appId} secretKey={entry.key} />}
          <Button
            type='button'
            size='sm'
            variant={urgent ? "default" : "ghost"}
            className='h-6 px-2 text-xs'
            onClick={() => onSet(entry.key)}
          >
            {entry.is_set ? "Rotate" : "Set"}
          </Button>
          {entry.is_set && <DeleteButton appId={appId} secretKey={entry.key} />}
        </div>
      </div>

      {entry.description && (
        <p className='mt-1 pl-5 text-muted-foreground text-xs'>{entry.description}</p>
      )}
    </li>
  );
};

const StateBadge = ({
  tone,
  children
}: {
  tone: "destructive" | "muted" | "outline";
  children: React.ReactNode;
}) => (
  <span
    className={cn(
      "shrink-0 rounded-sm px-1.5 py-px font-medium text-[10px]",
      tone === "destructive" && "bg-destructive/15 text-destructive",
      tone === "muted" && "bg-muted text-muted-foreground",
      tone === "outline" && "border border-border text-muted-foreground"
    )}
  >
    {children}
  </span>
);

/**
 * Fetches the value on demand and holds it in local state only.
 *
 * Deliberately not a react-query call: a decrypted secret in the query cache
 * outlives the row, survives navigation, and lands in any devtools pane looking
 * at the cache. Here it exists while the eye is open and is dropped when it
 * closes.
 */
const RevealButton = ({ appId, secretKey }: { appId: string; secretKey: string }) => {
  const [value, setValue] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const toggle = async () => {
    if (value !== null) {
      setValue(null);
      return;
    }
    setBusy(true);
    try {
      setValue(await CustomAppsService.revealSecret(appId, secretKey));
    } catch {
      toast.error(`Couldn't read ${secretKey}.`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <Button
        type='button'
        size='sm'
        variant='ghost'
        className='h-6 w-6 p-0'
        disabled={busy}
        onClick={toggle}
        title={value === null ? "Reveal value" : "Hide value"}
        aria-label={value === null ? `Reveal ${secretKey}` : `Hide ${secretKey}`}
      >
        {value === null ? <Eye className='size-3' /> : <EyeOff className='size-3' />}
      </Button>
      {value !== null && (
        <code
          className='max-w-40 truncate rounded-sm bg-muted px-1.5 py-px text-[11px]'
          title={value}
          data-testid={`admin-app-secret-value-${secretKey}`}
        >
          {value}
        </code>
      )}
    </>
  );
};

/** Two clicks, no dialog: the second click within the confirm window deletes.
 *  A modal for one destructive row in a dense operator list is more ceremony
 *  than the action deserves, but one click alone is too few. */
const DeleteButton = ({ appId, secretKey }: { appId: string; secretKey: string }) => {
  const [armed, setArmed] = useState(false);
  const disarm = useRef<ReturnType<typeof setTimeout>>(undefined);
  const del = useDeleteAppSecret(appId);

  // Collapsing the Secrets section unmounts an armed row mid-countdown, so the
  // timer has to be cancelled — otherwise it fires `setArmed` on a component
  // that is gone. Also cleared on re-arm, so two clicks don't leave two timers
  // racing to disarm the row.
  useEffect(() => () => clearTimeout(disarm.current), []);

  const click = () => {
    if (!armed) {
      setArmed(true);
      clearTimeout(disarm.current);
      disarm.current = setTimeout(() => setArmed(false), 3000);
      return;
    }
    del.mutate(secretKey, {
      onSuccess: () => toast.success(`Deleted ${secretKey}.`),
      onError: () => toast.error(`Couldn't delete ${secretKey}.`)
    });
  };

  return (
    <Button
      type='button'
      size='sm'
      variant='ghost'
      className={cn("h-6 px-2 text-xs", armed && "text-destructive")}
      disabled={del.isPending}
      onClick={click}
      title={armed ? "Click again to delete" : `Delete ${secretKey}`}
    >
      {armed ? "Confirm" : <Trash2 className='size-3' />}
    </Button>
  );
};
