import { useEffect, useState } from "react";
import { toast } from "sonner";
import { SecretInput } from "@/components/ui/SecretInput";
import { Button } from "@/components/ui/shadcn/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/shadcn/dialog";
import { Input } from "@/components/ui/shadcn/input";
import { Label } from "@/components/ui/shadcn/label";
import { useSetAppSecret } from "@/hooks/api/customApps/useAppSecrets";

/** Same charset the server enforces on the key half of `apps/<id>/<KEY>`. */
const KEY_PATTERN = /^[A-Za-z0-9_.-]+$/;

/**
 * Set a new key or rotate an existing one — the write path that did not exist
 * before, and still does not exist on the project-secrets API, which rejects the
 * `/` in an app-scoped name.
 *
 * Rotating locks the key field: an editable name on a rotation reads as "rename
 * this secret", which is not what saving would do — it would write a second key
 * and leave the first one live.
 */
export const SetSecretDialog = ({
  appId,
  secretKey,
  onClose
}: {
  appId: string;
  /** `null` = closed. `""` = adding a new key. A name = rotating that key. */
  secretKey: string | null;
  onClose: () => void;
}) => {
  const rotating = !!secretKey;
  const [key, setKey] = useState("");
  const [value, setValue] = useState("");
  const set = useSetAppSecret(appId);

  // Reset on each open so a previous value never lingers in the field — this
  // dialog holds a plaintext secret, so leaving one behind is worse than the
  // usual stale-form annoyance.
  useEffect(() => {
    if (secretKey === null) return;
    setKey(secretKey);
    setValue("");
  }, [secretKey]);

  const keyError =
    key.length > 0 && !KEY_PATTERN.test(key)
      ? "Use letters, numbers, underscores, hyphens and dots only."
      : null;
  const canSave = key.length > 0 && value.length > 0 && !keyError && !set.isPending;

  const save = () => {
    if (!canSave) return;
    set.mutate(
      { key, value },
      {
        onSuccess: () => {
          toast.success(rotating ? `Rotated ${key}.` : `Set ${key}.`);
          onClose();
        },
        // The server rejects a bad key by name; show what it said rather than a
        // generic failure, since the message names the rule that was broken.
        onError: (e: unknown) =>
          toast.error(e instanceof Error ? e.message : `Couldn't save ${key}.`)
      }
    );
  };

  return (
    <Dialog open={secretKey !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className='sm:max-w-[425px]' data-testid='admin-app-secret-dialog'>
        <DialogHeader>
          <DialogTitle className='text-sm'>
            {rotating ? `Rotate ${secretKey}` : "Add secret"}
          </DialogTitle>
          <DialogDescription className='text-xs'>
            Stored for this app only. Functions read it as <code>ctx.env.{key || "KEY"}</code> on
            their next run.
          </DialogDescription>
        </DialogHeader>

        <div className='grid gap-3 py-2'>
          <div className='grid gap-1.5'>
            <Label htmlFor='app-secret-key' className='text-xs'>
              Key
            </Label>
            <Input
              id='app-secret-key'
              value={key}
              disabled={rotating}
              onChange={(e) => setKey(e.target.value.trim())}
              placeholder='STRIPE_API_KEY'
              className='font-mono text-xs'
            />
            {keyError && <p className='text-destructive text-xs'>{keyError}</p>}
          </div>

          <div className='grid gap-1.5'>
            <Label htmlFor='app-secret-value' className='text-xs'>
              Value
            </Label>
            <SecretInput
              id='app-secret-value'
              value={value}
              onChange={(e) => setValue(e.target.value)}
              autoComplete='off'
              className='text-xs'
            />
          </div>
        </div>

        <DialogFooter>
          <Button type='button' variant='ghost' size='sm' className='text-xs' onClick={onClose}>
            Cancel
          </Button>
          <Button
            type='button'
            size='sm'
            className='text-xs'
            disabled={!canSave}
            onClick={save}
            data-testid='admin-app-secret-dialog-save'
          >
            {rotating ? "Rotate" : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
