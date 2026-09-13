// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CustomAppSummary } from "@/types/apps";
import { AppCard } from "./AppCard";

afterEach(cleanup);

beforeEach(() => {
  document.head.innerHTML = "";
});

const app = (over: Partial<CustomAppSummary> = {}): CustomAppSummary => ({
  id: "app-1",
  slug: "revenue",
  name: "Revenue",
  org_slug: "acme",
  url: "/customer-apps/acme/revenue/",
  published_at: "2026-07-30T00:00:00Z",
  visibility: "org",
  ...over
});

describe("AppCard access control", () => {
  // The whole point of the prop: the launcher passes it only for org owners and
  // admins. A member's card must not mention access at all — not the button, and
  // not the badge that would tell them which apps are locked down.
  it("shows nothing about access without onManageAccess", () => {
    render(<AppCard app={app({ visibility: "members" })} />);
    expect(screen.queryByRole("button", { name: /access/i })).toBeNull();
    expect(screen.queryByText(/restricted/i)).toBeNull();
  });

  it("offers the control to a manager", async () => {
    const onManageAccess = vi.fn();
    render(<AppCard app={app()} onManageAccess={onManageAccess} />);
    await userEvent.click(screen.getByRole("button", { name: /access/i }));
    expect(onManageAccess).toHaveBeenCalledWith(expect.objectContaining({ id: "app-1" }));
  });

  it("badges only the restricted app, and only for a manager", () => {
    const { rerender } = render(<AppCard app={app()} onManageAccess={vi.fn()} />);
    // An open app is the default and true of most cards, so it gets no chip —
    // otherwise every tile carries a badge that says nothing.
    expect(screen.queryByText(/restricted/i)).toBeNull();

    rerender(<AppCard app={app({ visibility: "members" })} onManageAccess={vi.fn()} />);
    expect(screen.getByText(/restricted/i)).toBeInTheDocument();
  });

  it("keeps the card a link to the app", () => {
    render(<AppCard app={app()} onManageAccess={vi.fn()} />);
    // The name's link stretches over the card. Adding a second action must not
    // cost the card its one-click navigation.
    expect(screen.getByRole("link", { name: /^Revenue/ })).toHaveAttribute(
      "href",
      "/customer-apps/acme/revenue/"
    );
  });
});

describe("AppCard opening behaviour", () => {
  /// The app opens in a tab of its own — never inside HQ. That is what keeps the
  /// app's URL in the address bar, so its query-string navigation state is
  /// linkable and a reload reloads the app instead of returning to HQ, and what
  /// keeps its history out of HQ's (the "Back lands two hops away" report).
  it("targets a per-app window rather than this one", () => {
    render(<AppCard app={app()} />);
    const link = screen.getByRole("link", { name: /^Revenue/ });
    // Org-scoped: slugs are unique within an org, but every org shares this
    // origin's window-name space.
    expect(link).toHaveAttribute("target", "oxy-app-acme-revenue");
    // A *named* target, not `_blank`: a second click re-targets the tab that is
    // already open instead of stacking a duplicate.
    expect(link.getAttribute("target")).not.toBe("_blank");
    // `rel` would defeat that reuse — a link with noopener/noreferrer is spec'd
    // to get a fresh browsing context every time.
    expect(link).not.toHaveAttribute("rel");
  });

  /// The card's explicit new-tab button went away with the dock, so this anchor
  /// is the only opener — and a screen-reader user has nothing else to tell them
  /// activating it leaves the tab.
  it("announces that it opens in a new tab", () => {
    render(<AppCard app={app()} />);
    expect(screen.getByRole("link", { name: "Revenue (opens in a new tab)" })).toBeInTheDocument();
  });

  /// Nothing intercepts the click. The card is a plain anchor, so cmd-click,
  /// middle-click, "open in new tab" and "copy link address" are all the
  /// browser's — re-implementing any of them is how in-app routers break them.
  it("leaves the click to the browser", () => {
    render(<AppCard app={app()} />);
    const link = screen.getByRole("link", { name: /^Revenue/ });
    const clicked = new MouseEvent("click", { bubbles: true, cancelable: true });
    link.dispatchEvent(clicked);
    expect(clicked.defaultPrevented).toBe(false);
  });

  /// Hover warms the app's HTML — which also pulls its entry chunks via the
  /// response's preload hints — so the click lands on something already parsed.
  /// A `rel=prefetch` link rather than a `fetch()`, so the serve path can tell
  /// a hover from an open and not record a view for it.
  it("prefetches the app on hover and on keyboard focus", async () => {
    render(<AppCard app={app()} />);
    const prefetches = () =>
      document.head.querySelectorAll('link[rel="prefetch"][href="/customer-apps/acme/revenue/"]');

    await userEvent.hover(screen.getByRole("link", { name: /^Revenue/ }));
    expect(prefetches()).toHaveLength(1);

    // Reaching the card by Tab has to be as fast as reaching it by mouse — and
    // must not queue a second link for the same URL.
    screen.getByRole("link", { name: /^Revenue/ }).focus();
    expect(prefetches()).toHaveLength(1);
  });
});

describe("AppCard art", () => {
  /// The launcher marks only its first row eager. Every other card stays lazy, so
  /// a long grid doesn't download images nobody has scrolled to.
  it("lazy-loads the art unless asked to load it eagerly", () => {
    const withArt = app({ art_url: "/customer-apps/acme/revenue/card.webp" });
    const { container, rerender } = render(<AppCard app={withArt} />);
    const art = () => container.querySelector('img[src="/customer-apps/acme/revenue/card.webp"]');
    expect(art()).toHaveAttribute("loading", "lazy");
    expect(art()).toHaveAttribute("decoding", "async");

    rerender(<AppCard app={withArt} eagerArt />);
    expect(art()).toHaveAttribute("loading", "eager");
  });
});
