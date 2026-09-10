/**
 * App-scoped secrets are ordinary project secrets whose name carries a reserved
 * prefix — `apps/<app_id>/<KEY>` — which is the namespace an Oxy Function reads
 * as `ctx.env.KEY`.
 *
 * This table shows every project secret, so those rows have always been in it,
 * rendered as the raw storage name: a uuid in the middle of a variable list,
 * belonging to an app nobody could identify from the row. These helpers turn one
 * back into the two facts it actually holds — which app, and which key.
 */

const APP_PREFIX = "apps/";

export interface AppSecretName {
  appId: string;
  /** The bare key, as the function sees it. */
  key: string;
}

/**
 * Split `apps/<app_id>/<KEY>` into its parts, or `null` for an ordinary
 * project secret.
 *
 * A key may itself contain dots and hyphens but never a slash (the server's
 * `validate_secret_name` rejects one), so the name has exactly three segments —
 * anything else is a name that merely starts with `apps/` and is left alone.
 */
export function parseAppSecretName(name: string): AppSecretName | null {
  if (!name.startsWith(APP_PREFIX)) return null;
  const parts = name.split("/");
  if (parts.length !== 3) return null;
  const [, appId, key] = parts;
  if (!appId || !key) return null;
  return { appId, key };
}

/** The storage name for a key in one app's namespace. */
export function appSecretName(appId: string, key: string): string {
  return `${APP_PREFIX}${appId}/${key}`;
}
