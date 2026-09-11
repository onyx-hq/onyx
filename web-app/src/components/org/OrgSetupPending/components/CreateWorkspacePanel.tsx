import { useState } from "react";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle
} from "@/components/ui/shadcn/card";
import WorkspaceCreator, {
  type WorkspaceCreationPhase
} from "@/components/workspaces/components/WorkspaceCreator";
import type { Organization } from "@/types/organization";

/**
 * The existing WorkspaceCreator flow in a card: pick → create → preparing,
 * after which the creator navigates into the new workspace's setup wizard.
 * Tracks the creator's phase only to swap the card header.
 */
export default function CreateWorkspacePanel({
  org,
  onBack
}: {
  /** Passed explicitly so creation never falls back to a stale store org. */
  org: Organization;
  onBack: () => void;
}) {
  const [phase, setPhase] = useState<WorkspaceCreationPhase>("create");
  const preparing = phase === "preparing";

  return (
    <Card className='w-full max-w-xl' data-testid='org-setup-pending-creator'>
      <CardHeader>
        <CardTitle className='text-lg'>
          {preparing ? "Preparing your workspace" : "Create a workspace"}
        </CardTitle>
        <CardDescription>
          {preparing
            ? "Hang tight — we're getting everything ready."
            : `For ${org.name}. Import from GitHub, start with a demo, or go blank.`}
        </CardDescription>
      </CardHeader>
      <CardContent>
        <WorkspaceCreator
          org={{ id: org.id, slug: org.slug }}
          onBack={onBack}
          onPhaseChange={setPhase}
        />
      </CardContent>
    </Card>
  );
}
