import { ShieldCheck } from "lucide-react";
// Direct, not via the barrel: a card that shows a badge shouldn't declare a
// dependency on the dialog and its pickers. (No bundle saving today — the app is a
// single chunk and the launcher imports the dialog anyway — so this is coupling,
// not bytes.)
import { AppAccessBadge } from "@/components/appAccess/AppAccessBadge";
import { AppArt } from "@/components/apps/AppArt";
import { AppMark } from "@/components/apps/AppMark";
import { Button } from "@/components/ui/shadcn/button";
import { appWindowName } from "@/libs/utils/appWindowName";
import { prefetchApp } from "@/libs/utils/prefetchApp";
import type { CustomAppSummary } from "@/types/apps";

export function AppCard({
  app,
  eagerArt = false,
  onManageAccess
}: {
  app: CustomAppSummary;
  /** Load the art immediately — set by the launcher for the cards on screen at first paint. */
  eagerArt?: boolean;
  /**
   * Open the access dialog for this app. Passed only for org owners and admins —
   * absent means no control renders at all, which is the right default for the
   * home page: everyone else's cards stay exactly as they were.
   */
  onManageAccess?: (app: CustomAppSummary) => void;
}) {
  // Hover and keyboard focus both mean "about to open this": a card reached by
  // Tab is one Enter away, and warming on focus is what keeps the keyboard path
  // as fast as the mouse one. `prefetchApp` uses a `rel=prefetch` link rather
  // than a `fetch()` precisely so the serve path can tell a hover from an open
  // and not record a view for it.
  const warm = () => prefetchApp(app.url);

  return (
    // The card is a div, not an anchor, because it holds a second action. The
    // name's link stretches over the whole card via `after:inset-0`, so the card
    // is still one big click target — but the access button is a sibling of that
    // link rather than nested inside it, which is what keeps it from navigating
    // (and keeps the markup valid).
    <div className='group relative flex flex-col gap-3 overflow-hidden rounded-lg border bg-card p-5 transition-colors focus-within:border-primary/50 hover:border-primary/50'>
      <AppArt artUrl={app.art_url} name={app.name} eager={eagerArt} />
      <div>
        {/* The mark sits beside the name — the same glyph the rail shows for this
            app — so the rail and the home cards read as one system. Kept off the
            art (which is the app's own screenshot) so it never clutters it. */}
        <div className='flex items-center gap-2'>
          <AppMark
            iconUrl={app.icon_url}
            name={app.name}
            size='md'
            testId={`launcher-app-card-mark-${app.slug}`}
          />
          <h3 className='font-semibold text-base text-card-foreground'>
            {/* A plain anchor with a real href and a target — no click handler.
                Every gesture people already know (cmd-click, middle-click, "open
                in new tab", "copy link address") stays the browser's, and the
                plain click lands in the app's own tab (`appWindowName`). */}
            <a
              href={app.url}
              target={appWindowName(app.org_slug, app.slug)}
              // The card's own new-tab affordance (an explicit external-link
              // button) went away with the dock, and this anchor is now the only
              // opener — so the warning it used to carry has to live here
              // (WCAG G201). An `aria-label` rather than a visually-hidden
              // suffix because the accessible-name calculation concatenates
              // sibling text nodes without a separator, which turns a hidden
              // " (opens in a new tab)" into "Revenueopens in a new tab". The
              // visible name is still the start of the label, so the two agree.
              aria-label={`${app.name} (opens in a new tab)`}
              data-testid={`launcher-app-card-${app.slug}`}
              className='after:absolute after:inset-0 after:content-[""]'
              // On the link, not the card: the stretched `::after` makes the
              // anchor's hit area the whole card, so this still fires wherever
              // the pointer enters — and the element being warmed is the one
              // that is about to be activated.
              onPointerEnter={warm}
              onFocus={warm}
            >
              {app.name}
            </a>
          </h3>
        </div>
        {app.description && (
          <p className='mt-1 line-clamp-2 text-muted-foreground text-sm'>{app.description}</p>
        )}
      </div>
      {(app.status || onManageAccess) && (
        <div className='mt-auto flex items-center gap-2 border-border/60 border-t pt-3'>
          {app.status && (
            <span className='min-w-0 flex-1 truncate text-muted-foreground/80 text-xs'>
              {app.status}
            </span>
          )}
          {onManageAccess && (
            // `z-10` lifts these above the stretched link's overlay — without it
            // the link swallows the click and the button silently navigates.
            <div className='relative z-10 ml-auto flex shrink-0 items-center gap-1.5'>
              {/* Only the restricted state gets a badge. "Whole org" is the
                  default and true of most cards, so labelling it would put a
                  chip on every tile to say nothing. */}
              {app.visibility === "members" && <AppAccessBadge visibility={app.visibility} />}
              <Button
                variant='ghost'
                size='sm'
                className='h-6 gap-1 px-1.5 text-xs'
                onClick={() => onManageAccess(app)}
              >
                <ShieldCheck className='size-3' aria-hidden />
                Access
              </Button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
