import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { OrganizationService } from "@/services/api/organizations";
import queryKeys from "../queryKey";

export const useOrgInvitations = (orgId: string, enabled = true) => {
  return useQuery({
    queryKey: queryKeys.org.invitations(orgId),
    queryFn: () => OrganizationService.listInvitations(orgId),
    enabled: enabled && !!orgId
  });
};

export const useCreateInvitation = () => {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ orgId, email, role }: { orgId: string; email: string; role: string }) =>
      OrganizationService.createInvitation(orgId, email, role),
    onSuccess: (_data, variables) => {
      queryClient.invalidateQueries({ queryKey: queryKeys.org.invitations(variables.orgId) });
    }
  });
};

export const useRevokeInvitation = () => {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ orgId, invitationId }: { orgId: string; invitationId: string }) =>
      OrganizationService.revokeInvitation(orgId, invitationId),
    onSuccess: (_data, variables) => {
      queryClient.invalidateQueries({ queryKey: queryKeys.org.invitations(variables.orgId) });
    }
  });
};

export const useAcceptInvitation = () => {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (token: string) => OrganizationService.acceptInvitation(token),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.org.list() });
      queryClient.invalidateQueries({ queryKey: queryKeys.org.myInvitations() });
    }
  });
};

export const useMyInvitations = (enabled = true) => {
  return useQuery({
    queryKey: queryKeys.org.myInvitations(),
    queryFn: () => OrganizationService.listMyInvitations(),
    enabled
  });
};
