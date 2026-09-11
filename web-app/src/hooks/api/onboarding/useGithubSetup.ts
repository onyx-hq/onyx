import { useQuery } from "@tanstack/react-query";
import useCurrentProjectBranch from "@/hooks/useCurrentProjectBranch";
import { type GithubSetupResponse, OnboardingService } from "@/services/api/onboarding";
import queryKeys from "../queryKey";

/** Workspace's missing `key_var` / warehouse `*_var` secrets. Not github-
 *  specific despite the name. Resolves presence against the workspace secret
 *  store (DB-only in cloud), not env vars. */
export default function useGithubSetup(enabled = true) {
  const { project } = useCurrentProjectBranch();
  const projectId = project.id;
  return useQuery<GithubSetupResponse, Error>({
    queryKey: queryKeys.onboarding.githubSetup(projectId),
    queryFn: () => OnboardingService.getGithubSetup(projectId),
    enabled
  });
}
