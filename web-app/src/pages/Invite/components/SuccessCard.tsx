import { CheckCircle2 } from "lucide-react";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/shadcn/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle
} from "@/components/ui/shadcn/card";
import { CenteredLayout } from "./CenteredLayout";

const REDIRECT_SECONDS = 5;

export function SuccessCard({ onDone }: { onDone: () => void }) {
  const [countdown, setCountdown] = useState(REDIRECT_SECONDS);

  useEffect(() => {
    if (countdown <= 0) {
      // Defer workspace selection to <PostLoginDispatcher>: it'll send the
      // user to the last-opened / first workspace, or to the org root's
      // "being set up" screen if the joined org has no workspace ready yet.
      onDone();
      return;
    }
    const timer = setTimeout(() => setCountdown((c) => c - 1), 1000);
    return () => clearTimeout(timer);
  }, [countdown, onDone]);

  return (
    <CenteredLayout>
      <Card className='w-full max-w-md'>
        <CardHeader className='text-center'>
          <div className='mb-4 flex justify-center'>
            <CheckCircle2 className='h-12 w-12 text-primary' />
          </div>
          <CardTitle className='text-2xl'>Invitation accepted</CardTitle>
          <CardDescription>Redirecting to your new organization in {countdown}…</CardDescription>
        </CardHeader>
        <CardContent>
          <Button className='w-full' onClick={onDone}>
            Go now
          </Button>
        </CardContent>
      </Card>
    </CenteredLayout>
  );
}
