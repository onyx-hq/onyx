/**
 * App publish tokens — long-lived bearer credentials for machine auth
 * (primarily `oxyc publish` in CI). Minted/listed/revoked by any Global
 * Admin at `/admin/publish-tokens`; a live token authenticates as its
 * minting admin **only on the customer-apps publish surface**. Backend:
 * `/api/admin/app-publish-tokens`.
 */

/** Metadata-only view for the list — never carries the plaintext or hash. */
export interface PublishToken {
  id: string;
  name: string;
  /** Display prefix (e.g. `oxypublish_ab12…`); the full token is never returned after creation. */
  token_prefix: string;
  created_by: string;
  created_at: string;
  last_used_at: string | null;
  revoked: boolean;
  revoked_at: string | null;
}

/** Create response — the only time the plaintext `token` is ever returned. */
export interface CreatedPublishToken {
  id: string;
  /** Plaintext token — shown once. Paste into a CI secret as `OXY_TOKEN`. */
  token: string;
  name: string;
  token_prefix: string;
  created_at: string;
}
