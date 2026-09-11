import { useEffect } from "react";
import { useSearchParams } from "react-router-dom";
import useSettingsDialog, { type SettingsSection } from "@/stores/useSettingsDialog";
import { CLOUD_NAV, LOCAL_NAV } from "./nav";

/** The query parameter a link uses to open the dialog at a section. */
export const SETTINGS_PARAM = "settings";

const KNOWN: ReadonlySet<string> = new Set(
  [...CLOUD_NAV, ...LOCAL_NAV].flatMap((group) => group.items.map((item) => item.value))
);

/**
 * The section a `?settings=` value names, or `null` for anything the dialog
 * does not have. Derived from the nav itself rather than a second list, so a
 * section added to the nav is linkable the same day.
 */
export function sectionFromParam(raw: string | null | undefined): SettingsSection | null {
  if (!raw) return null;
  return KNOWN.has(raw) ? (raw as SettingsSection) : null;
}

/**
 * `?settings=<section>` opens the dialog at that section once and strips the
 * param, so a refresh does not reopen it.
 *
 * Mounted wherever the dialog is — the workspace layout and the org's
 * "being set up" screen — so the link the custom-app shell already emits
 * (`…/home?settings=organization.general`) lands, and so does a link into an
 * org that has no workspace yet (`/<org>?settings=organization.crew`).
 * An unknown value is dropped rather than opening the wrong section; the
 * dialog's own gates decide what the viewer may see once it is open — a
 * valid section the viewer cannot see (a workspace section on an org with
 * none, a cloud section in local mode) falls to the first visible one, which
 * is why this hook needs no mode guard of its own.
 */
export function useSettingsDeepLink() {
  const [searchParams, setSearchParams] = useSearchParams();
  const open = useSettingsDialog((s) => s.open);
  useEffect(() => {
    const raw = searchParams.get(SETTINGS_PARAM);
    if (raw === null) return;
    const section = sectionFromParam(raw);
    if (section) open(section);
    // The functional form: another effect in the same commit (the Slack
    // return in the workspace layout) also strips a param, and two writes
    // from one snapshot would resurrect each other's deletion.
    setSearchParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        next.delete(SETTINGS_PARAM);
        return next;
      },
      { replace: true }
    );
  }, [searchParams, setSearchParams, open]);
}
