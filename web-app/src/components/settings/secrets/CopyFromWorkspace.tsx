import { useState } from "react";
import { toast } from "sonner";
import useSecrets from "@/hooks/api/secrets/useSecrets";
import useCurrentProjectBranch from "@/hooks/useCurrentProjectBranch";
import { SecretService } from "@/services/secretService";

/**
 * "There is already a workspace secret by this name — copy its value."
 *
 * ## Why a copy, and not inheritance
 *
 * The obvious version of this feature is a fallback: let `ctx.env.KEY` resolve
 * the workspace's `KEY` when the app has none. That was rejected. The workspace
 * secret store is where the *platform's* credentials live — warehouse and
 * ClickHouse passwords, LLM provider keys, Toast/QuickBooks tokens — and a
 * custom app runs code the tenant did not write. A fallback would hand every
 * app all of it the moment it declared nothing, silently, with the
 * `apps/<app_id>/` prefix (the only thing isolating one app from another, and
 * from the workspace) no longer load-bearing.
 *
 * Declaring the inheritance in `oxy-app.json` does not fix that: the manifest
 * ships inside the bundle, so a manifest that can name a workspace secret can
 * name `ANTHROPIC_API_KEY`. That is the same rule `webhook.secretVar` already
 * follows — a manifest cannot name another app's secret or a project-wide one.
 *
 * So the app gets its own copy, written once, by a human who can already read
 * both. What that costs is shared rotation: rotating the workspace secret does
 * not touch the app's, and the copy says so rather than letting someone assume
 * otherwise.
 *
 * ## Why only here
 *
 * This lives on the workspace settings surface and deliberately not in the staff
 * console's app dialog. Reading the value goes through the project-secrets
 * reveal route, which is `WorkspaceAdmin`-gated — the person using this can
 * already read both secrets. Offering it to staff would mean giving an app-admin
 * a read of the tenant's platform credentials without an assume-role session,
 * which is the widening this whole design refused.
 */
export const CopyFromWorkspace = ({
  name,
  onCopy
}: {
  /** The key being set, as typed. */
  name: string;
  /** Hands back the plaintext to drop into the value field. */
  onCopy: (value: string) => void;
}) => {
  const { project } = useCurrentProjectBranch();
  const { data } = useSecrets();
  const [busy, setBusy] = useState(false);

  // Exact match only, which is what confines this to workspace-scoped secrets:
  // an app's own rows are stored as `apps/<app_id>/KEY` and can never equal a
  // bare key. Copying one app's secret into another is not on offer — that is
  // the isolation this design exists to keep.
  const match = data?.secrets.find((s) => s.name === name.trim());
  if (!match) return null;

  const copy = async () => {
    setBusy(true);
    try {
      onCopy(await SecretService.revealSecret(project.id, match.id));
    } catch {
      toast.error(`Couldn't read the workspace value for ${match.name}.`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <p className='text-muted-foreground text-xs'>
      This workspace already has a secret named <code>{match.name}</code>.{" "}
      <button
        type='button'
        className='underline underline-offset-2 hover:text-foreground disabled:opacity-50'
        disabled={busy}
        onClick={copy}
        data-testid='secret-copy-from-workspace'
      >
        Copy its value
      </button>{" "}
      — a copy taken now, so rotating the workspace secret later won't change this one.
    </p>
  );
};
