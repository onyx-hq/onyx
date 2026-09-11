// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes, useLocation } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Organization, OrgRole } from "@/types/organization";
import OrgDispatcher from "./OrgDispatcher";

/**
 * `/:orgSlug` for an org with nothing ready. It used to redirect into a
 * self-serve onboarding wizard; orgs are provisioned for their members now, so
 * it renders "being set up" in place — and must never strand an admin who is
 * mid-way through creating a workspace.
 */

type Workspace = { id: string; status: "ready" | "cloning" | "failed" };
type User = { email: string; is_owner: boolean; is_app_admin: boolean };

const MEMBER_USER: User = { email: "crew@acme.test", is_owner: false, is_app_admin: false };

let org: Organization;
let workspaces: Workspace[];
let user: User;

vi.mock("@/hooks/api/organizations", () => ({
  useOrgs: () => ({ data: [org], isPending: false })
}));
vi.mock("@/hooks/api/workspaces/useWorkspaces", () => ({
  useAllWorkspaces: () => ({ data: workspaces, isPending: false, isError: false })
}));
vi.mock("@/hooks/api/users/useCurrentUser", () => ({
  default: () => ({ data: user, isPending: false })
}));
vi.mock("@/contexts/AuthContext", () => ({ useAuth: () => ({ logout: vi.fn() }) }));
vi.mock("@/components/OxyLogo", () => ({ default: () => null }));
vi.mock("@/components/settings/SettingsDialog", () => ({ default: () => null }));
vi.mock("@/components/settings/SettingsDialog/useSettingsDeepLink", () => ({
  useSettingsDeepLink: () => {}
}));
vi.mock("@/stores/useSettingsDialog", () => ({
  default: <T,>(selector: (s: { open: () => void }) => T) => selector({ open: vi.fn() })
}));
// The creator itself (GitHub / demo / blank, polling, wizard hand-off) is out
// of scope; a marker is enough to see whether the dispatcher kept it mounted.
vi.mock("@/components/org/OrgSetupPending/components/CreateWorkspacePanel", () => ({
  default: () => <div data-testid='creator' />
}));

function Location() {
  return <div data-testid='location'>{useLocation().pathname}</div>;
}

const renderAt = () =>
  render(
    <MemoryRouter initialEntries={["/acme"]}>
      <Routes>
        <Route path=':orgSlug' element={<OrgDispatcher />} />
        <Route path='*' element={<Location />} />
      </Routes>
    </MemoryRouter>
  );

const setRole = (role: OrgRole) => {
  org = { ...org, role };
};

beforeEach(() => {
  org = { id: "org-1", name: "Acme", slug: "acme", role: "member" };
  workspaces = [];
  user = MEMBER_USER;
});
afterEach(cleanup);

describe("OrgDispatcher — an org with nothing ready", () => {
  it("shows 'being set up' in place, not onboarding", () => {
    renderAt();
    expect(screen.getByTestId("org-setup-pending-title")).toHaveTextContent("Acme is being set up");
    expect(screen.queryByTestId("location")).toBeNull();
  });

  it("treats workspaces that are all still cloning as nothing ready", () => {
    workspaces = [{ id: "ws-1", status: "cloning" }];
    renderAt();
    expect(screen.getByTestId("org-setup-pending")).toBeInTheDocument();
  });

  it("offers a plain member no way to create a workspace", () => {
    renderAt();
    expect(screen.queryByTestId("org-setup-pending-create-workspace")).toBeNull();
    expect(screen.queryByTestId("org-setup-pending-open-settings")).toBeNull();
    expect(screen.getByTestId("org-setup-pending-log-out")).toBeInTheDocument();
  });

  it.each<OrgRole>(["owner", "admin"])(
    "offers an org %s organization settings, not the setup wizard",
    (role) => {
      // The card says the Oxygen team is setting the org up; a customer Owner
      // being handed "Create workspace" as well would contradict it.
      setRole(role);
      renderAt();
      expect(screen.queryByTestId("org-setup-pending-create-workspace")).toBeNull();
      expect(screen.getByTestId("org-setup-pending-open-settings")).toBeInTheDocument();
    }
  );

  it("does not offer staff who are a plain Member here a button the server refuses", () => {
    // Every creator endpoint takes OrgAdmin, which reads the org role only.
    user = { ...MEMBER_USER, is_app_admin: true };
    renderAt();
    expect(screen.queryByTestId("org-setup-pending-create-workspace")).toBeNull();
    // Org settings follow the org role, not platform standing.
    expect(screen.queryByTestId("org-setup-pending-open-settings")).toBeNull();
  });

  it("offers Create workspace to staff holding an Owner role (e.g. an assume-role session)", () => {
    user = { ...MEMBER_USER, is_app_admin: true };
    setRole("owner");
    renderAt();
    expect(screen.getByTestId("org-setup-pending-create-workspace")).toBeInTheDocument();
    expect(screen.getByTestId("org-setup-pending-open-settings")).toBeInTheDocument();
  });

  it("keeps the creator mounted when the new workspace turns ready", () => {
    // The created workspace flips the list to ready before the preparing screen
    // has handed off to the setup wizard. Navigating to the workspace here would
    // unmount the creator and skip that hand-off.
    user = { ...MEMBER_USER, is_app_admin: true };
    setRole("owner");
    const { rerender } = renderAt();
    fireEvent.click(screen.getByTestId("org-setup-pending-create-workspace"));
    expect(screen.getByTestId("creator")).toBeInTheDocument();

    workspaces = [{ id: "ws-new", status: "ready" }];
    rerender(
      <MemoryRouter initialEntries={["/acme"]}>
        <Routes>
          <Route path=':orgSlug' element={<OrgDispatcher />} />
          <Route path='*' element={<Location />} />
        </Routes>
      </MemoryRouter>
    );
    expect(screen.getByTestId("creator")).toBeInTheDocument();
    expect(screen.queryByTestId("location")).toBeNull();
  });
});

describe("OrgDispatcher — an org with a ready workspace", () => {
  it("still goes straight into it", () => {
    workspaces = [{ id: "ws-1", status: "ready" }];
    renderAt();
    expect(screen.getByTestId("location")).toHaveTextContent("/acme/workspaces/ws-1");
  });
});
