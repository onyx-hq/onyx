import type { AxiosError } from "axios";
import { XCircle } from "lucide-react";
import type React from "react";
import { useEffect, useRef, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { Button } from "@/components/ui/shadcn/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle
} from "@/components/ui/shadcn/card";
import { Spinner } from "@/components/ui/shadcn/spinner";
import { useDevLogin } from "@/hooks/auth/useDevLogin";
import ROUTES from "@/libs/utils/routes";
import { describeDevLoginFailure, serverReason } from "./describeDevLoginFailure";

/**
 * `/dev-login` — one navigation, one signed-in browser.
 *
 * The whole point of this page is that a tool driving a browser (Playwright
 * MCP, a scratch script) can `goto("/dev-login")` and be logged in, with no
 * OAuth popup and no magic-link inbox. It signs in on mount and redirects; the
 * UI below only ever shows while that round-trip is in flight, or when the
 * backend refuses.
 *
 * Query params — all optional:
 *   `email`     which configured identity to sign in as (default: the first)
 *   `as`        a seeded persona instead of an email, e.g. `/dev-login?as=member`
 *   `next`      same-origin path to land on, e.g. `/dev-login?next=/ide`
 *   `return_to` cross-origin destination, validated server-side
 */
const DevLogin: React.FC = () => {
  const [searchParams] = useSearchParams();
  const navigate = useNavigate();
  const email = searchParams.get("email") ?? undefined;
  const persona = searchParams.get("as") ?? undefined;
  const returnTo = searchParams.get("return_to") ?? undefined;
  const next = searchParams.get("next");
  // `setFailure` is a stable useState setter, so it stays correct even when the
  // callback that closes over it came from the discarded StrictMode pass — both
  // passes are the same fiber.
  const [failure, setFailure] = useState<AxiosError | null>(null);
  const { mutate: devLogin } = useDevLogin({
    returnTo,
    next,
    onFailure: (err) => setFailure(err as AxiosError)
  });
  // Latch, not `status === "idle"`: StrictMode runs mount → cleanup → mount in
  // one commit, before React Query has flushed `status` to "pending", so both
  // passes would read "idle" and sign in twice — two sessions, two redirects
  // racing each other. A ref is set synchronously, so the second pass sees it.
  const fired = useRef(false);

  useEffect(() => {
    if (fired.current) return;
    fired.current = true;
    devLogin({ email, as: persona });
  }, [devLogin, email, persona]);

  if (failure) {
    return (
      <div
        className='flex min-h-screen w-full items-center justify-center bg-background p-4'
        data-testid='dev-login-error'
      >
        <Card className='w-full max-w-md'>
          <CardHeader className='text-center'>
            <div className='mb-4 flex justify-center'>
              <XCircle className='h-12 w-12 text-destructive' />
            </div>
            <CardTitle className='text-2xl'>Dev sign-in unavailable</CardTitle>
            <CardDescription>
              {describeDevLoginFailure(
                failure.response?.status,
                email,
                serverReason(failure.response?.data)
              )}
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Button onClick={() => navigate(ROUTES.AUTH.LOGIN)} className='w-full'>
              Back to login
            </Button>
          </CardContent>
        </Card>
      </div>
    );
  }

  return (
    <div
      className='flex min-h-screen w-full items-center justify-center bg-background p-4'
      data-testid='dev-login-pending'
    >
      <div className='flex flex-col items-center gap-3'>
        <Spinner />
        <p className='text-muted-foreground text-sm'>
          Signing in{email || persona ? ` as ${email ?? persona}` : ""}…
        </p>
      </div>
    </div>
  );
};

export default DevLogin;
