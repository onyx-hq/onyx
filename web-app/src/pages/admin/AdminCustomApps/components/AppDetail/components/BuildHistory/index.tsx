import { AlertTriangle, GitBranch } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogTitle
} from "@/components/ui/shadcn/alert-dialog";
import { Badge } from "@/components/ui/shadcn/badge";
import { Button } from "@/components/ui/shadcn/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/shadcn/tooltip";
import { useAppBuilds, useRollbackApp } from "@/hooks/api/customApps/useAppBuilds";
import type { AppBuild } from "@/types/apps";

/** Normalize a git remote URL (`git@host:org/repo.git`, `https://…`, `ssh://…`)
 *  to an https browse URL, or null when unrecognized. */
function repoBrowseUrl(remote: string): string | null {
  const s = remote.trim();
  const ssh = s.match(/^git@([^:]+):(.+?)(?:\.git)?$/);
  if (ssh) return `https://${ssh[1]}/${ssh[2]}`;
  try {
    const u = new URL(s.replace(/^ssh:\/\//, "https://"));
    return `https://${u.host}${u.pathname.replace(/\.git$/, "")}`;
  } catch {
    return null;
  }
}

/** A GitHub-style source link: branch + short commit, linking to the commit
 *  (Vercel-style). Silent when the repo URL can't be normalized. */
const SourceLink = ({
  repo,
  sha,
  branch
}: {
  repo: string;
  sha: string;
  branch?: string | null;
}) => {
  const base = repoBrowseUrl(repo);
  const label = `${branch ? `${branch} · ` : ""}${sha.slice(0, 7)}`;
  const content = (
    <>
      <GitBranch className='size-3 shrink-0' />
      <span className='truncate'>{label}</span>
    </>
  );
  if (!base) {
    return (
      <span className='inline-flex items-center gap-1 text-muted-foreground/70 text-xs'>
        {content}
      </span>
    );
  }
  return (
    <a
      href={`${base}/commit/${sha}`}
      target='_blank'
      rel='noopener noreferrer'
      title={`${branch ? `${branch} @ ` : ""}${sha}`}
      onClick={(e) => e.stopPropagation()}
      className='inline-flex items-center gap-1 text-muted-foreground/80 text-xs hover:text-foreground hover:underline'
    >
      {content}
    </a>
  );
};

/**
 * Shown in `SourceLink`'s place when a build's repo + commit don't both
 * resolve. Reaching the code needs both: a repo without a commit points at a
 * branch, which moves; a commit without a repo has nowhere to resolve.
 *
 * The silence this replaces was the whole problem: a build with no provenance
 * rendered identically to one whose link simply sat further down the row, so
 * an app nobody could trace looked exactly like an app nobody had scrolled
 * to. `oxyc publish` warns about this at publish time; this is where you find
 * the ones that already shipped.
 *
 * Names which half is missing — this row has both fields, so telling an
 * operator "no source recorded" when the repo is right there would send them
 * looking for the wrong thing.
 */
const IncompleteSource = ({ repo, sha }: { repo?: string | null; sha?: string | null }) => {
  const hasRepo = !!repo?.trim();
  const hasSha = !!sha?.trim();
  const label = hasRepo ? "No commit recorded" : hasSha ? "No repo recorded" : "No source recorded";
  const detail = hasRepo
    ? "This build records a git repo but no commit, so the link points at a branch — which moves, and won't identify the code that is running."
    : hasSha
      ? "This build records a commit but no git repo, so there's nowhere to resolve that sha."
      : "This build records no git repo and no commit, so there's no way to get from the running app back to its code.";
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span
          className='inline-flex w-fit items-center gap-1 text-amber-600 text-xs dark:text-amber-400'
          data-testid='admin-app-build-no-source'
        >
          <AlertTriangle className='size-3 shrink-0' />
          <span>{label}</span>
        </span>
      </TooltipTrigger>
      <TooltipContent className='max-w-xs'>
        {detail} Re-publish from the app's git checkout to attach provenance.
      </TooltipContent>
    </Tooltip>
  );
};

const RTF = new Intl.RelativeTimeFormat("en", { numeric: "auto" });
const TIME_UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 31_536_000_000],
  ["month", 2_592_000_000],
  ["day", 86_400_000],
  ["hour", 3_600_000],
  ["minute", 60_000],
  ["second", 1_000]
];

/** "3 hours ago" from an ISO string. Falls back to "" on a bad date. */
function timeAgo(iso: string): string {
  const ms = new Date(iso).getTime();
  if (Number.isNaN(ms)) return "";
  const diff = ms - Date.now();
  const abs = Math.abs(diff);
  for (const [unit, msPer] of TIME_UNITS) {
    if (abs >= msPer || unit === "second") return RTF.format(Math.round(diff / msPer), unit);
  }
  return "";
}

/** Git-sha-style short id; leaves already-short ids untouched. */
const shortId = (s: string) => (s.length > 10 ? s.slice(0, 7) : s);

/**
 * Deployment console for a custom app: the live + draft channel pointers
 * at a glance, the full versioned build history (who published each, when),
 * and one-click "Make Live" of any retained build (a pure pointer move
 * server-side). Empty for legacy s3/local/v0 apps never published via
 * `oxyc publish`.
 */
export const BuildHistory = ({ appId }: { appId: string }) => {
  const { data, isLoading, error } = useAppBuilds(appId);
  const rollback = useRollbackApp();
  const [confirm, setConfirm] = useState<AppBuild | null>(null);
  const builds = data?.builds;

  if (isLoading) {
    return <p className='text-muted-foreground text-xs'>Loading deployments…</p>;
  }
  if (error) {
    return <p className='text-destructive text-xs'>Couldn't load deployments.</p>;
  }
  if (!builds || builds.length === 0) {
    return (
      <p className='text-muted-foreground text-xs'>
        No deployments yet — this app hasn't been published via the pipeline (oxyc publish).
      </p>
    );
  }

  const live = builds.find((b) => b.is_published);
  const draft = builds.find((b) => b.is_draft);
  const draftAhead = draft && (!live || draft.id !== live.id);

  const makeLive = (b: AppBuild) => {
    rollback.mutate(
      { id: appId, buildId: b.id },
      {
        onSuccess: () => {
          toast.success(`Build ${shortId(b.build_id)} is now live.`);
          setConfirm(null);
        },
        onError: (e) =>
          toast.error(e instanceof Error ? e.message : "Couldn't change the live build")
      }
    );
  };

  return (
    <div className='flex flex-col gap-4'>
      {/* Channel pointers at a glance */}
      <div className='grid @lg:grid-cols-2 grid-cols-1 gap-2'>
        <ChannelCard
          label='Live'
          build={live}
          tone='live'
          promotedByEmail={data?.promoted_by_email}
          promotedAt={data?.promoted_at}
        />
        <ChannelCard label='Draft' build={draftAhead ? draft : undefined} tone='draft' />
      </div>

      {/* Full history */}
      <div className='flex flex-col gap-2'>
        <h3 className='font-medium text-xs'>Build history</h3>
        <ul className='flex flex-col gap-1'>
          {builds.map((b) => (
            <li
              key={b.id}
              className='flex items-center justify-between gap-3 rounded-md border border-border bg-card px-3 py-2'
            >
              <div className='flex min-w-0 flex-col'>
                <div className='flex items-center gap-2'>
                  <span className='font-mono text-xs' title={b.build_id}>
                    {shortId(b.build_id)}
                  </span>
                  {b.is_published && <Badge className='h-5'>Live</Badge>}
                  {b.is_draft && !b.is_published && (
                    <Badge variant='secondary' className='h-5'>
                      Draft
                    </Badge>
                  )}
                </div>
                <span
                  className='truncate text-muted-foreground text-xs'
                  title={new Date(b.created_at).toLocaleString()}
                >
                  {timeAgo(b.created_at)}
                  {b.published_by_email ? ` · ${b.published_by_email}` : ""}
                </span>
                {b.source_repo?.trim() && b.commit_sha?.trim() ? (
                  <SourceLink repo={b.source_repo} sha={b.commit_sha} branch={b.source_branch} />
                ) : (
                  <IncompleteSource repo={b.source_repo} sha={b.commit_sha} />
                )}
              </div>
              {!b.is_published && (
                <Button
                  size='sm'
                  variant='outline'
                  disabled={rollback.isPending}
                  onClick={() => setConfirm(b)}
                >
                  Make live
                </Button>
              )}
            </li>
          ))}
        </ul>
      </div>

      <AlertDialog open={!!confirm} onOpenChange={(o) => !o && setConfirm(null)}>
        <AlertDialogContent className='gap-6 sm:max-w-md'>
          <div className='flex flex-col gap-2'>
            <AlertDialogTitle>Make this build live?</AlertDialogTitle>
            <AlertDialogDescription>
              Customers will immediately see build{" "}
              <span className='font-mono'>{confirm ? shortId(confirm.build_id) : ""}</span>
              {live ? (
                <>
                  {" "}
                  instead of the current live build{" "}
                  <span className='font-mono'>{shortId(live.build_id)}</span>
                </>
              ) : null}
              . You can switch back here at any time.
            </AlertDialogDescription>
          </div>
          <div className='flex justify-end gap-2'>
            <Button variant='ghost' disabled={rollback.isPending} onClick={() => setConfirm(null)}>
              Cancel
            </Button>
            <Button disabled={rollback.isPending} onClick={() => confirm && makeLive(confirm)}>
              {rollback.isPending ? "Switching…" : "Make live"}
            </Button>
          </div>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
};

/** Compact summary of which build a channel currently points at. */
const ChannelCard = ({
  label,
  build,
  tone,
  promotedByEmail,
  promotedAt
}: {
  label: string;
  build: AppBuild | undefined;
  tone: "live" | "draft";
  promotedByEmail?: string | null;
  promotedAt?: string | null;
}) => (
  <div className='flex flex-col gap-1 rounded-md border border-border bg-card px-3 py-2'>
    <div className='flex items-center gap-2'>
      <span
        className={`size-2 rounded-full ${tone === "live" ? "bg-primary" : "bg-muted-foreground/50"}`}
        aria-hidden
      />
      <span className='font-medium text-muted-foreground text-xs uppercase tracking-wide'>
        {label}
      </span>
    </div>
    {build ? (
      <>
        <span className='font-mono text-xs' title={build.build_id}>
          {shortId(build.build_id)}
        </span>
        <span className='text-muted-foreground text-xs'>
          built {timeAgo(build.created_at)}
          {build.published_by_email ? ` · ${build.published_by_email}` : ""}
        </span>
        {tone === "live" && promotedAt && (
          <span className='text-muted-foreground text-xs'>
            promoted {timeAgo(promotedAt)}
            {promotedByEmail ? ` · ${promotedByEmail}` : ""}
          </span>
        )}
      </>
    ) : (
      <span className='text-muted-foreground text-xs'>
        {tone === "live" ? "Not live yet" : "Same as live"}
      </span>
    )}
  </div>
);
