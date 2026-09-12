import type { CreatedPublishToken, PublishToken } from "@/types/publishTokens";
import { apiClient } from "./axios";

/**
 * App publish tokens (machine-auth bearer for `oxyc publish`). Managed by
 * any Global Admin; mounted at `/api/admin/app-publish-tokens`. Tokens
 * cannot manage tokens — this surface is session-credentialed only.
 */
export const PublishTokensService = {
  async list(): Promise<PublishToken[]> {
    const response = await apiClient.get("/admin/app-publish-tokens");
    return response.data;
  },

  async create(name: string): Promise<CreatedPublishToken> {
    const response = await apiClient.post("/admin/app-publish-tokens", { name });
    return response.data;
  },

  async revoke(id: string): Promise<void> {
    await apiClient.post(`/admin/app-publish-tokens/${id}/revoke`);
  }
};
