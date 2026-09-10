/**
 * What an admin app view *is*, expressed as query params.
 *
 * ## The rule
 *
 * A **location** goes in the URL; a personal **layout preference** stays in
 * `localStorage`. "Bookkeeping, draft channel, on mobile, builds section open,
 * showing the vendor screen" is a place an operator wants to send someone. Dock
 * right-vs-bottom and whether their dossier is pinned is how *they* like to sit,
 * and a shared link that rearranged the recipient's panels would be a bug.
 *
 * That split is why `dock`, `dossierPinned` and the per-section collapse state
 * are untouched by this module and stay in `usePersistentState`.
 *
 * ## What is deliberately absent
 *
 * The debug panel's selected request. Its id is a per-session counter over
 * whatever calls that frame happened to make, so `?req=3` names a different
 * request on every load — a link that reproduces the wrong row is worse than
 * one that reproduces nothing. The panel's open/closed state is layout, so it
 * follows the rule above.
 */

export type Device = "desktop" | "tablet" | "mobile";
export type ChannelView = "published" | "draft";
export type SectionId =
  | "status"
  | "builds"
  | "access"
  | "functions"
  | "secrets"
  | "availability"
  | "logs"
  | "activity"
  | "settings";

export interface AppViewState {
  device: Device;
  channel: ChannelView;
  /** Dossier section to scroll to and force open. `null` = leave the operator's
   *  stored collapse state alone, which is the default for a plain visit. */
  section: SectionId | null;
  /** Selected Oxy Function, by name. */
  fn: string | null;
  /**
   * Where the preview is pointed, as a path within the app's own bundle
   * (`/?vendor=ubereats&month=2026-07`). Stored relative so a link survives the
   * bundle moving between the subpath and subdomain surfaces, and so the param
   * stays legible in the admin URL bar.
   */
  preview: string | null;
}

const DEVICES: readonly Device[] = ["desktop", "tablet", "mobile"];
const SECTIONS: readonly SectionId[] = [
  "status",
  "builds",
  "access",
  "functions",
  "secrets",
  "availability",
  "logs",
  "activity",
  "settings"
];

/** Query-param names, in one place so the reader and the writer cannot drift. */
export const PARAM = {
  device: "device",
  channel: "channel",
  section: "section",
  fn: "fn",
  preview: "preview"
} as const;

/**
 * Read the view out of a query string.
 *
 * Every field validates against what the UI can actually render and falls back
 * rather than throwing: a URL is user input, and a hand-edited `?device=phone`
 * should show the desktop preview, not an error boundary.
 *
 * `channel` has no static default — an app with nothing published must open on
 * Draft, or the toolbar selects a disabled option and the iframe requests a
 * bundle that does not exist. So the caller passes what "no param" means.
 */
export function readAppViewState(
  params: URLSearchParams,
  defaults: { channel: ChannelView }
): AppViewState {
  const device = params.get(PARAM.device);
  const channel = params.get(PARAM.channel);
  const section = params.get(PARAM.section);
  const fn = params.get(PARAM.fn);
  const preview = params.get(PARAM.preview);

  return {
    device: DEVICES.includes(device as Device) ? (device as Device) : "desktop",
    channel: channel === "draft" || channel === "published" ? channel : defaults.channel,
    section: SECTIONS.includes(section as SectionId) ? (section as SectionId) : null,
    fn: fn || null,
    // Must stay a path within the app. An absolute URL here would let a link
    // point the preview at any origin, which is a redirect the admin console
    // would be lending its frame to.
    preview: preview?.startsWith("/") && !preview.startsWith("//") ? preview : null
  };
}

/**
 * Apply a patch to an existing query string, dropping params that are back at
 * their default.
 *
 * Dropping matters more than it looks: without it, every visit accumulates
 * `?device=desktop&channel=published`, and the URL an operator copies is noise
 * around the one param they actually changed. It also keeps the *table's* own
 * params (filter, sort, group — `useAppsTable` owns those on the same string)
 * untouched, so leaving the detail returns to the list the operator left.
 */
export function writeAppViewState(
  current: URLSearchParams,
  patch: Partial<AppViewState>,
  defaults: { channel: ChannelView }
): URLSearchParams {
  const next = new URLSearchParams(current);
  const set = (key: string, value: string | null, isDefault: boolean) => {
    if (value === null || isDefault) next.delete(key);
    else next.set(key, value);
  };

  if ("device" in patch) set(PARAM.device, patch.device ?? null, patch.device === "desktop");
  if ("channel" in patch)
    set(PARAM.channel, patch.channel ?? null, patch.channel === defaults.channel);
  if ("section" in patch) set(PARAM.section, patch.section ?? null, false);
  if ("fn" in patch) set(PARAM.fn, patch.fn ?? null, false);
  if ("preview" in patch) {
    // "/" is the app's own root — the state a fresh preview is already in.
    set(PARAM.preview, patch.preview ?? null, patch.preview === "/");
  }
  return next;
}

/**
 * The preview's absolute URL reduced to the path this module stores, or `null`
 * when it is outside the app (which a link must not be able to reproduce).
 *
 * `base` is the app's own bundle prefix — `window.__OXY_APP__.basePath` as the
 * serve path sets it, which is the same string on the subpath and subdomain
 * surfaces.
 */
export function toPreviewPath(absolute: string, base: string): string | null {
  try {
    const url = new URL(absolute);
    const prefix = base.endsWith("/") ? base : `${base}/`;
    if (!url.pathname.startsWith(prefix)) return null;
    // `_oxy_preview` is the cache-buster `LivePreview` adds to force a fresh
    // navigation. It is noise in a shared link, and it would come back a
    // different number anyway.
    url.searchParams.delete(PREVIEW_NONCE_PARAM);
    const rest = url.pathname.slice(prefix.length);
    return `/${rest}${url.search}${url.hash}`;
  } catch {
    return null;
  }
}

/** The cache-buster `LivePreview` puts on the iframe URL. Named here because
 *  `toPreviewPath` has to strip exactly the same one. */
export const PREVIEW_NONCE_PARAM = "_oxy_preview";

/**
 * The inverse: a stored path back to a URL the frame can be pointed at, or
 * `null` when it does not resolve inside the app after all.
 *
 * The reader's `startsWith("/") && !startsWith("//")` rejects the absolute and
 * protocol-relative forms, which is the pair that looks like an attack. It does
 * not catch `..`: `/../../../admin/apps` passes both checks and `URL`
 * normalises it straight out of the bundle prefix. Same-origin, so the blast
 * radius is small — but the module's whole claim is that a link cannot aim this
 * frame somewhere else, and that would.
 *
 * Checked by round trip rather than by string surgery: `toPreviewPath` already
 * owns "is this inside the app", so anything that does not survive there is not
 * a location this module is willing to name.
 */
export function fromPreviewPath(path: string, base: string, origin: string): string | null {
  try {
    const resolved = new URL(`${base.endsWith("/") ? base.slice(0, -1) : base}${path}`, origin);
    return toPreviewPath(resolved.toString(), base) === null ? null : resolved.toString();
  } catch {
    return null;
  }
}
