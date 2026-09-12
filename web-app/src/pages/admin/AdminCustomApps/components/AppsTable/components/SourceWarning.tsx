import { AlertTriangle } from "lucide-react";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/shadcn/tooltip";

/**
 * Marks an app whose currently-serving build doesn't record both a git repo
 * and a commit — either half alone can't reach the code.
 *
 * Renders nothing when the app is traceable, so a clean list stays clean and
 * the mark reads as an exception rather than decoration. That's the whole
 * job: scanning the list should be how you find the apps nobody can maintain,
 * without opening each one.
 *
 * The list only carries a boolean, so the copy here stays accurate for all
 * three shapes rather than naming a half that might be present. The build
 * history in the detail pane has the fields and says exactly which is
 * missing.
 *
 * `oxyc publish` warns about this at publish time too. This is the surface for
 * the ones that already shipped — including every app published before that
 * warning existed.
 */
export const SourceWarning = ({ unrecorded }: { unrecorded?: boolean }) => {
  if (!unrecorded) return null;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span
          role='img'
          className='inline-flex shrink-0 text-amber-600 dark:text-amber-400'
          data-testid='admin-app-source-unrecorded'
          aria-label='Incomplete source for the running build'
        >
          <AlertTriangle className='size-3' />
        </span>
      </TooltipTrigger>
      <TooltipContent className='max-w-xs'>
        Incomplete source — the build this app is serving doesn't record both a git repo and a
        commit, so there's no reliable way to get from the running app back to its code. Open the
        app to see which half is missing.
      </TooltipContent>
    </Tooltip>
  );
};
