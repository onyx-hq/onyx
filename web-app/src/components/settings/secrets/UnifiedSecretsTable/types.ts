import type { EnvSecret, Secret } from "@/types/secret";

/** A single row in the secrets table — either a DB-stored secret or an env var. */
export interface UnifiedRow {
  key: string;
  /** What the row is called on screen. For an app secret this is the bare key
   *  (`STRIPE_API_KEY`), not the `apps/<uuid>/` name it is stored under. */
  name: string;
  /** Set when the row is an app-scoped secret — the app it belongs to. */
  app?: { id: string; name: string };
  source: "secret" | "dot_env" | "environment" | "not_set";
  referencedBy?: string | null;
  maskedValue?: string;
  secretInfo?: Secret;
  envInfo?: EnvSecret;
}

export const SOURCE_CONFIG = {
  secret: { label: "Secret", className: "border-info/30 bg-info/10 text-info" },
  dot_env: { label: ".env", className: "border-warning/30 bg-warning/10 text-warning" },
  environment: { label: "env", className: "border-success/30 bg-success/10 text-success" },
  not_set: {
    label: "not set",
    className: "border-destructive/30 bg-destructive/10 text-destructive"
  }
} as const;

export const DOTS = "••••••••••••••";
