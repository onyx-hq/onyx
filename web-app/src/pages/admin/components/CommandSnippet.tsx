import { Check, Copy } from "lucide-react";
import { useCopyTimeout } from "@/components/automation/output/useCopyTimeout";
import { cn } from "@/libs/shadcn/utils";

/**
 * A single-line, copy-to-clipboard command chip — a monospace snippet with a
 * trailing copy button that flips to a check. For precomposed CLI commands the
 * operator grabs and runs (e.g. an `oxyc publish` invocation).
 */
export const CommandSnippet = ({ command, className }: { command: string; className?: string }) => {
  const { copied, handleCopy } = useCopyTimeout();
  return (
    <div
      className={cn(
        "flex min-w-0 items-center gap-1 rounded border bg-muted/40 py-0.5 pr-0.5 pl-1.5",
        className
      )}
    >
      <code className='min-w-0 flex-1 truncate font-mono text-[11px] text-muted-foreground'>
        {command}
      </code>
      <button
        type='button'
        aria-label='Copy command'
        title={command}
        onClick={(e) => {
          e.stopPropagation();
          handleCopy(command);
        }}
        className='inline-flex size-5 shrink-0 items-center justify-center rounded text-muted-foreground transition-colors hover:bg-accent hover:text-foreground'
      >
        {copied ? <Check className='size-3 text-primary' /> : <Copy className='size-3' />}
      </button>
    </div>
  );
};
