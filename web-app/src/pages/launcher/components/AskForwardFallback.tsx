import { askPlaceholder } from "@/components/Ask/askPlaceholder";
import ChatPanel from "@/components/Chat/ChatPanel";
import { cn } from "@/libs/shadcn/utils";
import { getGreeting } from "@/libs/utils/greeting";
import useCurrentOrg from "@/stores/useCurrentOrg";
import useCurrentWorkspace from "@/stores/useCurrentWorkspace";

/**
 * Empty-state home for workspaces with zero published custom apps
 * (every `oxy init` user): the classic greeting + composer, so the
 * launcher never renders a dead grid. Submitting navigates to the full
 * thread page (the Ask dock is the surface for in-context asks).
 */
export function AskForwardFallback({
  shouldDisableChat,
  hasSetupSteps
}: {
  shouldDisableChat: boolean;
  /** Whether the setup toast lists steps this viewer can take. */
  hasSetupSteps: boolean;
}) {
  const orgName = useCurrentOrg((s) => s.org?.name);
  // The workspace switcher navigates to this workspace's ROOT (the
  // launcher), which is the SAME route/element across a switch — so this
  // component instance is reused rather than remounted. Key ChatPanel by
  // workspace id so a typed draft can't survive the switch (see #2962).
  const wsId = useCurrentWorkspace((s) => s.workspace?.id);
  return (
    <div className='flex flex-1 flex-col items-center justify-center gap-6 px-4 sm:gap-10'>
      <p className='text-balance text-center text-xl sm:text-3xl'>
        {getGreeting()}! How can I assist you?
      </p>
      <div className='flex w-full max-w-4xl flex-col items-center gap-3'>
        {shouldDisableChat && (
          <p
            className='text-center text-muted-foreground/50 text-xs'
            data-testid='launcher-chat-locked-reason'
          >
            {hasSetupSteps
              ? "Complete the setup steps above to start chatting."
              : "Chat isn't set up for this workspace yet."}
          </p>
        )}
        <div
          className={cn(
            "w-full",
            shouldDisableChat && "pointer-events-none select-none opacity-40"
          )}
        >
          <ChatPanel
            key={wsId}
            lockMode='ask'
            hideAgentPicker
            placeholderOverride={askPlaceholder(orgName)}
          />
        </div>
      </div>
    </div>
  );
}
