import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { OrganizationService } from "@/services/api/organizations";
import queryKeys from "../queryKey";

export const useOrgs = () => {
  return useQuery({
    queryKey: queryKeys.org.list(),
    queryFn: () => OrganizationService.listOrgs()
  });
};

export const useUpdateOrg = () => {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ orgId, data }: { orgId: string; data: { name?: string; slug?: string } }) =>
      OrganizationService.updateOrg(orgId, data),
    onSuccess: (_data, variables) => {
      queryClient.invalidateQueries({ queryKey: queryKeys.org.item(variables.orgId) });
      queryClient.invalidateQueries({ queryKey: queryKeys.org.list() });
    }
  });
};

export const useUploadOrgLogo = () => {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ orgId, file }: { orgId: string; file: File }) =>
      OrganizationService.uploadLogo(orgId, file),
    onSuccess: (_data, variables) => {
      queryClient.invalidateQueries({ queryKey: queryKeys.org.item(variables.orgId) });
      queryClient.invalidateQueries({ queryKey: queryKeys.org.list() });
    }
  });
};

export const useDeleteOrgLogo = () => {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (orgId: string) => OrganizationService.deleteLogo(orgId),
    onSuccess: (_data, orgId) => {
      queryClient.invalidateQueries({ queryKey: queryKeys.org.item(orgId) });
      queryClient.invalidateQueries({ queryKey: queryKeys.org.list() });
    }
  });
};

export const useDeleteOrg = () => {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (orgId: string) => OrganizationService.deleteOrg(orgId),
    onSuccess: () => {
      // Drop the cached list entirely — a stale entry containing the deleted
      // org can cause PostLoginDispatcher's pickOrg to briefly re-select it
      // before the refetch lands, which looks like "delete didn't navigate".
      queryClient.removeQueries({ queryKey: queryKeys.org.list() });
    }
  });
};
