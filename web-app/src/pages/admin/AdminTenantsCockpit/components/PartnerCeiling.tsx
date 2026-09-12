import { Switch } from "@/components/ui/shadcn/switch";
import { useSetPartnerCapabilities } from "@/hooks/api/adminPartners";
import useCurrentUser from "@/hooks/api/users/useCurrentUser";
import { cn } from "@/libs/shadcn/utils";
import type { AdminPartnerCapabilities } from "@/types/adminPartners";

const HINTS: Record<keyof AdminPartnerCapabilities, string> = {
  manage_members: "invite / role / remove in client orgs",
  manage_apps: "publish and unpublish only — no data access",
  develop_apps: "sensitive — query the client's data (app dev / oxyc proxy)",
  view_audit: "read the subtree audit log",
  manage_billing: "sensitive — Global Owner only",
  manage_secrets: "sensitive — Global Owner only",
  create_orgs: "sensitive — onboard client orgs, which mints billable tenants",
  manage_org_settings: "rename / configure a client org"
};

const ROWS: {
  key: keyof AdminPartnerCapabilities;
  label: string;
  /** The backend rejects a non-Owner granting these. */
  ownerOnly?: boolean;
  /** Granting this changes nothing yet — no endpoint consumes it. Say so, so an
   *  operator doesn't believe they just gave a partner billing access. */
  notEnforced?: boolean;
}[] = [
  { key: "manage_members", label: "Manage members" },
  { key: "manage_apps", label: "Publish apps" },
  { key: "develop_apps", label: "App data access" },
  { key: "create_orgs", label: "Onboard clients" },
  { key: "manage_org_settings", label: "Change org settings" },
  { key: "view_audit", label: "View audit" },
  { key: "manage_billing", label: "Manage billing", ownerOnly: true, notEnforced: true },
  { key: "manage_secrets", label: "Manage secrets", ownerOnly: true, notEnforced: true }
];

/**
 * The **ceiling** — what Oxy permits this partner AT ALL, not what any one person
 * there can do. The partner's own admin hands out roles *inside* this; a role
 * permission the ceiling doesn't grant is inert, so turning a switch off here
 * silently disarms it for everyone at the partner.
 */
export default function PartnerCeiling({
  orgId,
  capabilities
}: {
  orgId: string;
  capabilities: AdminPartnerCapabilities;
}) {
  const { data: me } = useCurrentUser();
  const isOwner = !!me?.is_owner;
  const setCaps = useSetPartnerCapabilities(orgId);

  return (
    <div className='divide-y rounded-lg border'>
      {ROWS.map((r) => {
        const locked = r.ownerOnly && !isOwner;
        return (
          <div key={r.key} className='flex items-center justify-between gap-3 px-3 py-2'>
            <div className={cn("min-w-0", locked && "opacity-60")}>
              <div className='flex items-center gap-1.5'>
                <span className='font-medium text-xs'>{r.label}</span>
                {r.notEnforced && (
                  <span className='rounded-sm bg-muted px-1 py-0 text-[10px] text-muted-foreground uppercase tracking-wide'>
                    not enforced yet
                  </span>
                )}
              </div>
              <div className='truncate text-muted-foreground text-xs'>{HINTS[r.key]}</div>
            </div>
            <Switch
              checked={capabilities[r.key]}
              disabled={locked || setCaps.isPending}
              title={locked ? "Global Owner only" : undefined}
              onCheckedChange={() =>
                setCaps.mutate({ ...capabilities, [r.key]: !capabilities[r.key] })
              }
            />
          </div>
        );
      })}
    </div>
  );
}
