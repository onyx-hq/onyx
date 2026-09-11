import { apiClient } from "./axios";

export interface OnboardingResult {
  workspace_type: "demo" | "new" | "github";
  workspace_id: string;
}

export interface OnboardingResetRequest {
  /** Secret names to delete (e.g. `ANTHROPIC_API_KEY`). */
  secret_names: string[];
  /** Warehouse database names to remove from `config.yml`. The backend also
   * deletes the associated `password_var` secret for each entry. */
  database_names: string[];
  /** Model names to remove from `config.yml`. The backend also deletes the
   * associated `key_var` secret for each entry. */
  model_names: string[];
  /** File paths (relative to the workspace root) to delete. */
  file_paths: string[];
  /** Directory paths (relative to the workspace root) to recursively delete.
   * Used for wiping generated trees such as `.databases/<warehouse>/`. */
  directory_paths: string[];
}

export interface OnboardingResetResponse {
  secrets_deleted: string[];
  databases_removed: string[];
  models_removed: string[];
  files_deleted: string[];
  directories_deleted: string[];
  warnings: string[];
}

interface GithubSetupKeyVar {
  var_name: string;
  vendor: string;
  sample_model_name?: string;
}

interface GithubSetupMissingVar {
  field: string;
  var_name: string;
  required: boolean;
}

interface GithubSetupWarehouse {
  name: string;
  dialect: string;
  missing_vars: GithubSetupMissingVar[];
}

interface GithubSetupModel {
  name: string;
  /** Null for keyless vendors (e.g. Ollama). */
  key_var: string | null;
}

export interface GithubSetupResponse {
  missing_llm_key_vars: GithubSetupKeyVar[];
  warehouses: GithubSetupWarehouse[];
  /** Lets callers map an agent's `model` to its `key_var`, so they can
   *  decide whether a *specific* key is missing rather than any-of. */
  models?: GithubSetupModel[];
}

export interface TestLlmKeyResponse {
  /** True when the provider accepted the key. */
  success: boolean;
  /** Actionable error message when `success` is false; absent on success. */
  message?: string;
}

export interface UploadWarehouseFilesResponse {
  /** Subdir (relative to workspace root) the files were written to. */
  subdir: string;
  /** Paths of files that were successfully written, relative to workspace root. */
  files: string[];
  /** Files that were rejected (unsupported extension, hidden name, etc.). */
  skipped: { name: string; reason: string }[];
}

export class OnboardingService {
  static async setupDemo(orgId: string, name?: string): Promise<OnboardingResult> {
    const response = await apiClient.post<OnboardingResult>(`/orgs/${orgId}/onboarding/demo`, {
      name
    });
    return response.data;
  }

  static async setupNew(orgId: string, name?: string): Promise<OnboardingResult> {
    const response = await apiClient.post<OnboardingResult>(`/orgs/${orgId}/onboarding/new`, {
      name
    });
    return response.data;
  }

  static async setupGitHub(
    orgId: string,
    namespaceId: string,
    repoId: number,
    branch: string,
    name?: string,
    subdir?: string
  ): Promise<OnboardingResult> {
    const response = await apiClient.post<OnboardingResult>(`/orgs/${orgId}/onboarding/github`, {
      namespace_id: namespaceId,
      repo_id: repoId,
      branch,
      name,
      subdir: subdir || undefined
    });
    return response.data;
  }

  /**
   * Inspect the imported repo's `config.yml` and return any LLM API keys and
   * warehouse `*_var` secrets that still need to be provided before the
   * workspace can be queried. Drives the GitHub-mode onboarding flow.
   */
  static async getGithubSetup(workspaceId: string): Promise<GithubSetupResponse> {
    const response = await apiClient.get<GithubSetupResponse>(
      `/${workspaceId}/onboarding/github-setup`
    );
    return response.data;
  }

  /**
   * Revert the server-side side effects of a partial onboarding run (secrets,
   * warehouse entries in config.yml, and generated files). Each list is handled
   * idempotently — missing entries are silently skipped.
   */
  static async resetOnboarding(
    workspaceId: string,
    manifest: OnboardingResetRequest
  ): Promise<OnboardingResetResponse> {
    const response = await apiClient.post<OnboardingResetResponse>(
      `/${workspaceId}/onboarding/reset`,
      manifest
    );
    return response.data;
  }

  /**
   * Verify an LLM API key against the provider before saving it as a workspace
   * secret. Hits the provider's `/v1/models` endpoint (no token cost) and
   * reports whether the key is accepted. Used by the onboarding flow so users
   * see "invalid key" before they pick a warehouse and tables, instead of
   * deep inside the agentic build phase.
   */
  static async testLlmKey(
    workspaceId: string,
    provider: "anthropic" | "openai",
    apiKey: string
  ): Promise<TestLlmKeyResponse> {
    const response = await apiClient.post<TestLlmKeyResponse>(
      `/${workspaceId}/onboarding/test-llm-key`,
      { provider, api_key: apiKey }
    );
    return response.data;
  }

  /**
   * Upload CSV / Parquet data files into a subdirectory of the workspace root
   * (default `.db/`). Used by the DuckDB onboarding step so users can start
   * from a completely empty workspace — no need to pre-populate files on disk.
   *
   * The backend rejects anything other than `.csv` / `.parquet`, caps per-file
   * and aggregate size, and returns 409 on filename collision.
   */
  static async uploadWarehouseFiles(
    workspaceId: string,
    files: File[],
    subdir?: string,
    onProgress?: (loaded: number, total: number) => void
  ): Promise<UploadWarehouseFilesResponse> {
    const form = new FormData();
    if (subdir) form.append("subdir", subdir);
    for (const file of files) {
      form.append("file", file, file.name);
    }
    const response = await apiClient.post<UploadWarehouseFilesResponse>(
      `/${workspaceId}/onboarding/upload-warehouse-files`,
      form,
      {
        onUploadProgress: (e) => {
          if (onProgress && e.total) onProgress(e.loaded, e.total);
        }
      }
    );
    return response.data;
  }
}
