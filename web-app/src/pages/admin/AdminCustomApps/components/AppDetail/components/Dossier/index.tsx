import { ChevronRight, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/shadcn/button";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger
} from "@/components/ui/shadcn/collapsible";
import type { CustomApp } from "@/types/apps";
// One declaration of the section ids, in the module that also validates them
// off the query string. Two structurally-identical copies type-check happily,
// which is exactly how they drift.
import type { SectionId } from "../../appViewState";
import type { DockMode } from "../../dock";
import { usePersistentState } from "../../usePersistentState";
import { Activity } from "../Activity";
import { AppAccessPane } from "../AppAccessPane";
import { AppInfo } from "../AppInfo";
// `AppLogs`, not `Logs`: web-app/.gitignore has a bare `logs` pattern, which
// git matches case-insensitively on macOS — a directory named `Logs/` is
// silently untracked, `git add -A` reports nothing, and the branch builds
// locally while CI fails on the missing import.
import { AppLogs } from "../AppLogs";
import { AppSettings } from "../AppSettings";
import { Availability } from "../Availability";
import { BuildHistory } from "../BuildHistory";
import { Functions } from "../Functions";
import { Secrets, SecretsBadge } from "../Secrets";
import { DockControls } from "./DockControls";
import { isSectionOpen } from "./sectionOpen";

export { DockControls };

/** The dossier renders in three places; only the one inside `AppDetail` has a
 *  URL to write to. The others show the same content read-only rather than
 *  each inventing their own selection state. */
const noop = () => undefined;

/**
 * Open by default: the two an operator opens the panel *for*. The rest answer
 * follow-up questions, so they start collapsed — five expanded sections is what
 * made this column a scroll marathon.
 */
const DEFAULT_OPEN: Record<SectionId, boolean> = {
  status: true,
  builds: true,
  // Collapsed by default: most apps are open to their whole org, so the badge
  // inside answers the question without the section needing to be expanded.
  access: false,
  functions: false,
  // Collapsed like its neighbours: the header badge counts the keys this app is
  // missing, which is the whole question when nothing is wrong, and expanding
  // is only needed to act on one.
  secrets: false,
  // Open by default, unlike its neighbours: it is the section an operator opens
  // the dossier FOR during an incident, and a collapsed "is it up" answer is a
  // click away from being no answer at all.
  availability: true,
  // Collapsed: the verdict above answers "is it broken"; this answers "why",
  // which is the second question, and it is a long panel.
  logs: false,
  activity: false,
  settings: false
};

const SECTIONS_STORAGE_KEY = "admin-app-dossier-sections";

const reviveOpenState = (raw: unknown): Record<SectionId, boolean> | null => {
  if (typeof raw !== "object" || raw === null) return null;
  const stored = raw as Record<string, unknown>;
  // Merge over the defaults so a key added in a later build still appears.
  return Object.fromEntries(
    Object.entries(DEFAULT_OPEN).map(([id, fallback]) => [
      id,
      typeof stored[id] === "boolean" ? stored[id] : fallback
    ])
  ) as Record<SectionId, boolean>;
};

/**
 * The dossier's own title strip — dock switcher on the right, exactly where
 * DevTools puts it. Separate from `DetailToolbar` on purpose: that row belongs
 * to the *preview*, and it's already full.
 */
export const DossierHeader = ({
  dock,
  onDockChange,
  onClose
}: {
  dock: DockMode;
  onDockChange: (next: DockMode) => void;
  onClose?: () => void;
}) => (
  <div className='flex h-9 shrink-0 items-center gap-2 border-b bg-background px-2'>
    <span className='ml-1 min-w-0 flex-1 truncate font-mono text-[11px] text-muted-foreground uppercase tracking-wider'>
      Details
    </span>
    <DockControls value={dock} onChange={onDockChange} />
    {onClose && (
      <Button
        variant='ghost'
        size='icon'
        className='size-6'
        onClick={onClose}
        aria-label='Hide details'
      >
        <X className='size-3.5' />
      </Button>
    )}
  </div>
);

/**
 * The stacked dossier sections, shared by every placement (side column, bottom
 * drawer, popped-out window, narrow-screen sheet) so they can't drift.
 *
 * Sections reflow by the *panel's* width, not the viewport's — a container
 * query, because the panel is resizable and can be 400px or 1400px wide at the
 * same viewport size. Docked right it reads as one column; docked bottom the
 * same content fans out to two or three, which is the point of that placement:
 * one screenful instead of five, without label/value pairs stranded at opposite
 * ends of a 1400px row.
 */
export const DossierBody = ({
  app,
  focusSection,
  fn,
  onFnChange
}: {
  app: CustomApp;
  focusSection?: SectionId | null;
  /** `?fn=` — the open function inside the Functions section. */
  fn?: string | null;
  onFnChange?: (name: string | null) => void;
}) => {
  const [open, setOpen] = usePersistentState(SECTIONS_STORAGE_KEY, DEFAULT_OPEN, reviveOpenState);
  const scroller = useRef<HTMLDivElement>(null);

  // A `?section=` in the admin URL names the section the link was sent about,
  // so it opens and scrolls into view — as an override held for this URL, NOT
  // by writing the operator's stored collapse map. A colleague's link should
  // answer its question and leave the panel the way this operator keeps it;
  // persisting it would silently re-file their layout every time they followed
  // one.
  // Dismissal, so the override is not a one-way door. Without it the section
  // named by `?section=` cannot be closed at all: the trigger writes `false`
  // into the stored map, the unconditional override re-opens it, and the
  // collapsible reads as broken — the only escape being to edit the URL.
  //
  // Stored as *which* focus was dismissed rather than a boolean, so following a
  // second link opens its section without needing an effect to reset a flag.
  const [dismissedFor, setDismissedFor] = useState<SectionId | null>(null);

  useEffect(() => {
    if (!focusSection) return;
    // After the open lands, not with it: a collapsed section has no height to
    // scroll to, so measuring in the same frame targets the row above it.
    const id = requestAnimationFrame(() => {
      scroller.current
        ?.querySelector(`[data-testid="admin-app-dossier-section-${focusSection}"]`)
        ?.scrollIntoView({ block: "start", behavior: "smooth" });
    });
    return () => cancelAnimationFrame(id);
  }, [focusSection]);

  // One source of each section's id — it drives the open state, the toggle, and
  // the testid, and repeating it three times per call site is how those drift.
  const section = (id: SectionId) => ({
    id,
    // The URL's section is forced open on top of the stored map rather than
    // into it — see the effect above — until the operator closes it.
    open: isSectionOpen(open[id], id, focusSection, dismissedFor),
    onOpenChange: (next: boolean) => {
      if (!next && id === focusSection) setDismissedFor(id);
      setOpen((prev) => ({ ...prev, [id]: next }));
    }
  });

  return (
    <div ref={scroller} className='@container min-h-0 flex-1 overflow-auto'>
      <div className='grid @3xl:grid-cols-2 @5xl:grid-cols-3 grid-cols-1 items-start gap-x-8'>
        <DossierSection {...section("status")} title='Status & manifest'>
          <AppInfo app={app} />
        </DossierSection>
        <DossierSection {...section("builds")} title='Build history'>
          <div className='p-4 pt-0'>
            <BuildHistory appId={app.id} />
          </div>
        </DossierSection>
        <DossierSection {...section("access")} title='Access'>
          <AppAccessPane app={app} />
        </DossierSection>
        <DossierSection {...section("functions")} title='Functions'>
          <div className='p-4 pt-0'>
            <Functions appId={app.id} selected={fn ?? null} onSelect={onFnChange ?? noop} />
          </div>
        </DossierSection>
        <DossierSection
          {...section("secrets")}
          title='Secrets'
          badge={<SecretsBadge appId={app.id} />}
        >
          <div className='p-4 pt-0'>
            <Secrets appId={app.id} />
          </div>
        </DossierSection>
        <DossierSection {...section("availability")} title='Availability'>
          <Availability orgSlug={app.org_slug} appSlug={app.slug} />
        </DossierSection>
        <DossierSection {...section("logs")} title='Logs'>
          <AppLogs orgSlug={app.org_slug} appSlug={app.slug} />
        </DossierSection>
        <DossierSection {...section("activity")} title='Activity'>
          <Activity appId={app.id} />
        </DossierSection>
        <DossierSection {...section("settings")} title='Settings'>
          <AppSettings app={app} />
        </DossierSection>
      </div>
    </div>
  );
};

/**
 * A collapsible block in the dossier. The header doubles as the disclosure
 * control, so the section costs one 32px row when it's closed and nothing at
 * all in horizontal space.
 *
 * Also a container in its own right: a section's *cell* is a third of the panel
 * once the grid splits, so anything inside that reflows (Activity's stat tiles,
 * BuildHistory's channel cards) must measure this box, not the whole panel —
 * otherwise a wide panel tells a 360px cell to lay out four columns.
 */
const DossierSection = ({
  id,
  title,
  badge,
  open,
  onOpenChange,
  children
}: {
  /**
   * The section's stable identity — same key as its open/closed state. Drives
   * `data-testid`, so a section stays targetable when its `title` copy changes.
   */
  id: SectionId;
  title: string;
  /** Optional status shown beside the title, so a section that is collapsed by
   *  default can still say something is wrong inside it. */
  badge?: React.ReactNode;
  open: boolean;
  onOpenChange: (next: boolean) => void;
  children: React.ReactNode;
}) => (
  <Collapsible
    open={open}
    onOpenChange={onOpenChange}
    data-testid={`admin-app-dossier-section-${id}`}
    className='@container min-w-0 border-border/60 border-b'
  >
    <CollapsibleTrigger className='group flex w-full items-center gap-1.5 px-4 py-2 text-left transition-colors hover:bg-muted/40'>
      <ChevronRight className='size-3 shrink-0 text-muted-foreground transition-transform group-data-[state=open]:rotate-90' />
      <span className='font-medium text-[10px] text-muted-foreground uppercase tracking-[0.16em]'>
        {title}
      </span>
      {badge}
    </CollapsibleTrigger>
    <CollapsibleContent>{children}</CollapsibleContent>
  </Collapsible>
);
