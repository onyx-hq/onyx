import { isAxiosError } from "axios";
import { Check, type LucideIcon, ShieldCheck, Terminal } from "lucide-react";
import { useState } from "react";
import { Button } from "@/components/ui/shadcn/button";
import { Input } from "@/components/ui/shadcn/input";
import {
  ResizableHandle,
  ResizablePanel,
  ResizablePanelGroup
} from "@/components/ui/shadcn/resizable";
import { Spinner } from "@/components/ui/shadcn/spinner";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/shadcn/tooltip";
import { cn } from "@/libs/shadcn/utils";

export function formatGrantedAt(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleDateString();
}

/**
 * Slim top readout for the Access tab — mirrors the Apps tab's FleetStrip so the
 * two admin surfaces read as one cockpit. Total orgs, how many granted access,
 * and the granted-workspace count at a glance.
 */
export const AccessStrip = ({
  orgs,
  withAccess,
  workspaces
}: {
  orgs: number;
  /** Orgs that have locked Oxy out of at least one workspace. */
  withAccess: number;
  workspaces: number;
}) => (
  <div className='flex flex-wrap items-center gap-x-5 gap-y-1.5 border-b bg-muted/20 px-4 py-2 text-xs'>
    <AccessStat value={orgs} label={orgs === 1 ? "org" : "orgs"} />
    <AccessStat value={withAccess} label='locked out' />
    <AccessStat value={workspaces} label='workspaces' />
    <span className='ml-auto flex items-center gap-1.5 font-mono text-[11px] text-muted-foreground'>
      <ShieldCheck className='size-3.5' /> Oxy access is the default
    </span>
  </div>
);

const AccessStat = ({ value, label }: { value: number; label: string }) => (
  <span className='flex items-center gap-1.5'>
    <span className='font-semibold text-foreground text-xs tabular-nums'>{value}</span>
    <span className='text-muted-foreground'>{label}</span>
  </span>
);

/** Icon-only row action with a tooltip — the dense cockpit affordance that
 *  replaces the labeled buttons (which overflowed a narrow detail pane). */
export const RowAction = ({
  icon: Icon,
  label,
  onClick,
  disabled
}: {
  icon: LucideIcon;
  label: string;
  onClick: () => void;
  /** Disabled actions stay VISIBLE (with an explanatory tooltip) so an operator
   *  can see the capability exists and why it's unavailable — e.g. locked out. */
  disabled?: boolean;
}) => (
  <Tooltip>
    <TooltipTrigger asChild>
      <Button
        variant='ghost'
        size='icon'
        disabled={disabled}
        className='size-6 text-muted-foreground hover:text-foreground'
        onClick={onClick}
        aria-label={label}
      >
        <Icon className='size-3.5' />
      </Button>
    </TooltipTrigger>
    <TooltipContent side='bottom'>{label}</TooltipContent>
  </Tooltip>
);

/** Copies the workspace's `oxyc publish` command (shown in the tooltip) and
 *  flips to a check — the publish snippet, collapsed to one dense icon. */
export const CopyPublishAction = ({ workspaceId }: { workspaceId: string }) => {
  const [copied, setCopied] = useState(false);
  const cmd = `oxyc publish --env production --project ${workspaceId}`;
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(cmd);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    } catch (err) {
      console.error("Failed to copy publish command", err);
    }
  };
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          variant='ghost'
          size='icon'
          className='size-6 text-muted-foreground hover:text-foreground'
          onClick={copy}
          aria-label='Copy publish command'
        >
          {copied ? <Check className='size-3.5 text-primary' /> : <Terminal className='size-3.5' />}
        </Button>
      </TooltipTrigger>
      <TooltipContent side='bottom' className='font-mono text-xs'>
        {cmd}
      </TooltipContent>
    </Tooltip>
  );
};

/** Resizable list/detail skeleton shared by both panes (matches the Apps tab). */
export const MasterDetail = ({
  list,
  detail
}: {
  list: React.ReactNode;
  detail: React.ReactNode;
}) => (
  <ResizablePanelGroup direction='horizontal' className='min-h-0 flex-1'>
    <ResizablePanel defaultSize={32} minSize={20} maxSize={50}>
      {list}
    </ResizablePanel>
    <ResizableHandle withHandle />
    <ResizablePanel defaultSize={68} minSize={40}>
      {detail}
    </ResizablePanel>
  </ResizablePanelGroup>
);

export const PaneSearch = ({
  value,
  onChange,
  placeholder
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder: string;
}) => (
  <div className='border-border border-b p-2'>
    <Input
      value={value}
      onChange={(e) => onChange(e.target.value)}
      placeholder={placeholder}
      className='h-7 text-xs'
    />
  </div>
);

/** A selectable row in the list pane: name + mono subtitle, with the cockpit's
 *  left accent bar on the active row and a trailing slot for a count chip.
 *  `muted` dims the identity for rows that represent an absence (no access). */
export const ListRow = ({
  active,
  onClick,
  title,
  subtitle,
  trailing,
  muted
}: {
  active: boolean;
  onClick: () => void;
  title: string;
  subtitle: string;
  trailing?: React.ReactNode;
  muted?: boolean;
}) => (
  <button
    type='button'
    onClick={onClick}
    aria-current={active}
    className={cn(
      "group relative flex w-full items-center gap-2 border-border/50 border-b py-1.5 pr-2 pl-3 text-left transition-colors",
      active ? "bg-primary/10" : "hover:bg-muted/50"
    )}
  >
    <span
      aria-hidden
      className={cn(
        "absolute inset-y-1 left-0 w-0.5 rounded-full bg-primary transition-opacity",
        active ? "opacity-100" : "opacity-0"
      )}
    />
    <span className={cn("min-w-0 flex-1", muted && "opacity-55")}>
      <span className='block truncate font-medium text-xs'>{title}</span>
      <span className='block truncate font-mono text-[11px] text-muted-foreground'>{subtitle}</span>
    </span>
    {trailing}
  </button>
);

export const GrantsLoading = () => (
  <div className='flex h-full items-center justify-center'>
    <Spinner className='size-5' />
  </div>
);

export const EmptyHint = ({ title, body }: { title: string; body: string }) => (
  <div className='flex h-full flex-col items-center justify-center gap-3 bg-muted/20 px-6 text-center'>
    <div className='flex size-12 items-center justify-center rounded-full border bg-background shadow-sm'>
      <ShieldCheck className='size-5 text-muted-foreground' />
    </div>
    <div>
      <p className='font-medium text-foreground text-xs'>{title}</p>
      <p className='mt-1 max-w-sm text-muted-foreground text-xs'>{body}</p>
    </div>
  </div>
);

/** 403-aware error block matching the Apps tab's allow-list message. */
export const GrantsError = ({ error }: { error: unknown }) => (
  <div className='mx-auto max-w-2xl p-6'>
    <div className='rounded-lg border border-destructive/30 bg-destructive/5 p-6 text-center'>
      {isAxiosError(error) && error.response?.status === 403 ? (
        <>
          <p className='font-medium text-destructive text-xs'>
            Your account isn't on the custom-apps allow list.
          </p>
          <p className='mt-2 text-muted-foreground text-xs'>
            Add your email to the oxy backend's{" "}
            <code className='rounded bg-muted px-1 py-0.5 font-mono'>OXY_GLOBAL_ADMINS</code> and
            refresh.
          </p>
        </>
      ) : (
        <p className='text-destructive text-xs'>Failed to load Oxy-access grants.</p>
      )}
    </div>
  </div>
);
