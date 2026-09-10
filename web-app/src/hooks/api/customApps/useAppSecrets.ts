import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";

import { CustomAppsService } from "@/services/api/customApps";
import { errMessage } from "../errMessage";
import queryKeys from "../queryKey";

/**
 * The app's secrets: what its active build declares (`env` in `oxy-app.json`,
 * plus every function's `webhook.secretVar`), unioned with what is actually
 * stored under `apps/<app_id>/`. Values are never in this payload.
 */
export function useAppSecrets(id: string | undefined) {
  return useQuery({
    queryKey: queryKeys.customApps.secrets(id ?? ""),
    queryFn: () => CustomAppsService.listSecrets(id as string),
    enabled: !!id
  });
}

/** Create or rotate one key, then refetch so the row flips to Set. */
export function useSetAppSecret(id: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ key, value }: { key: string; value: string }) =>
      CustomAppsService.setSecret(id, key, value),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: queryKeys.customApps.secrets(id) })
  });
}

/**
 * Create or rotate an app-scoped secret from the workspace's own settings,
 * rather than the staff console. Invalidates the project secrets list, which is
 * what that surface renders — an app secret is a project secret with a reserved
 * name, so it appears in the same table.
 */
export function useSetWorkspaceAppSecret(workspaceId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ appId, key, value }: { appId: string; key: string; value: string }) =>
      CustomAppsService.setWorkspaceAppSecret(workspaceId, appId, key, value),
    onSuccess: () =>
      queryClient.invalidateQueries({ queryKey: queryKeys.secret.list(workspaceId) }),
    // The dialog calling this catches and only `console.error`s, on the
    // assumption — true of `useCreateSecret`, and it had to become true here —
    // that the hook owns the toast. Without this a rejected key (400), an app
    // outside the workspace (404), or a missing role (403) all leave the dialog
    // open with the value still in the field and nothing saying why.
    //
    // The server's message names the rule that was broken, so it beats a generic
    // string; `errMessage` falls back when there is not one.
    onError: (error) => toast.error(errMessage(error, "Failed to save app secret"))
  });
}

export function useDeleteAppSecret(id: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (key: string) => CustomAppsService.deleteSecret(id, key),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: queryKeys.customApps.secrets(id) })
  });
}
