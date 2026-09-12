import { Check, ChevronDown, Copy, ShieldCheck } from "lucide-react";
import type { ComponentType, ReactNode } from "react";
import { useState } from "react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/shadcn/badge";
import { Button } from "@/components/ui/shadcn/button";
import { Card, CardContent } from "@/components/ui/shadcn/card";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger
} from "@/components/ui/shadcn/collapsible";

const TOKEN_WORKFLOW = `name: Publish to Oxy
on:
  push:
    branches: [main]
  workflow_dispatch:

jobs:
  publish:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: pnpm/action-setup@v4
        with:
          version: 10
      - uses: actions/setup-node@v4
        with:
          node-version: 20
      # Builds per oxy-app.json (default: pnpm install, pnpm build), bundles any
      # Oxy Functions, and publishes. Pin the @oxy-hq/cli version in a real workflow.
      - run: npx --yes @oxy-hq/cli publish --env production
        env:
          OXY_TOKEN: \${{ secrets.OXY_TOKEN }}
`;

/**
 * "Use in CI" — how to wire a publish credential into GitHub Actions.
 *
 * Each method is collapsible so the section stays a couple of lines until you open
 * one. Trusted publishing (no stored secret) leads and opens by default; the token
 * path — and its long workflow YAML — stays folded.
 */
export default function CiInstructions({ showTokenPath = true }: { showTokenPath?: boolean }) {
  return (
    <Card className='mb-6'>
      <CardContent className='space-y-2 p-4'>
        <div>
          <h2 className='font-semibold text-xs'>Use in CI (GitHub Actions)</h2>
          <p className='mt-0.5 text-muted-foreground text-xs'>
            {showTokenPath
              ? "Trusted publishing is recommended — it stores no secret."
              : "Publish with trusted publishing — no stored secret, minted per run."}
          </p>
        </div>

        <Method title='Trusted publishing' badge='Recommended' icon={ShieldCheck} defaultOpen>
          <p className='text-muted-foreground text-xs'>
            Run the generator in your app directory. It writes a workflow that mints a short-lived,
            app-scoped credential via GitHub OIDC — nothing is stored in the repo — and prints the
            command that registers that workflow as the app&apos;s publisher.
          </p>
          <CopyBlock label='Run locally' code='oxyc init-ci --app <org-slug>/<app-slug>' oneLine />
          <p className='text-muted-foreground text-xs'>
            Then gate who can publish: add required reviewers to the{" "}
            <span className='font-mono'>oxy-publish</span> environment (Settings → Environments).
          </p>
        </Method>

        {showTokenPath && (
          <Method title='Token secret' badge="If you can't use OIDC">
            <ol className='ml-4 list-decimal space-y-1 text-muted-foreground text-xs'>
              <li>Create a token above and copy it.</li>
              <li>
                Add it as an Actions secret named{" "}
                <span className='font-mono text-foreground'>OXY_TOKEN</span> (Settings → Secrets and
                variables → Actions).
              </li>
              <li>
                Save this workflow as{" "}
                <span className='font-mono text-foreground'>.github/workflows/publish.yml</span>.
              </li>
            </ol>
            <CopyBlock label='publish.yml' code={TOKEN_WORKFLOW} />
          </Method>
        )}
      </CardContent>
    </Card>
  );
}

/** A collapsible how-to method — a one-line header until you open it. */
function Method({
  title,
  badge,
  icon: Icon,
  defaultOpen,
  children
}: {
  title: string;
  badge?: string;
  icon?: ComponentType<{ className?: string }>;
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  return (
    <Collapsible defaultOpen={defaultOpen} className='rounded-lg border'>
      <CollapsibleTrigger className='group flex w-full items-center gap-1.5 px-3 py-2 text-left hover:bg-muted/40'>
        {Icon ? <Icon className='size-4 text-primary' /> : null}
        <span className='font-medium text-xs'>{title}</span>
        {badge ? (
          <Badge variant='outline' className='px-1.5 py-0 text-[10px] text-muted-foreground'>
            {badge}
          </Badge>
        ) : null}
        <ChevronDown className='ml-auto size-4 text-muted-foreground transition-transform group-data-[state=open]:rotate-180' />
      </CollapsibleTrigger>
      <CollapsibleContent className='space-y-2 border-t px-3 py-3'>{children}</CollapsibleContent>
    </Collapsible>
  );
}

/** A code block with a copy-to-clipboard button, mirroring the created-token dialog. */
function CopyBlock({ label, code, oneLine }: { label: string; code: string; oneLine?: boolean }) {
  const [copied, setCopied] = useState(false);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(code);
      setCopied(true);
      toast.success(`${label} copied`);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      toast.error("Couldn't copy to clipboard");
    }
  };

  return (
    <div className='relative'>
      <pre
        className={`overflow-x-auto rounded-md border border-border/60 bg-muted/40 p-3 pr-10 font-mono text-xs ${
          oneLine ? "whitespace-pre" : ""
        }`}
      >
        <code>{code}</code>
      </pre>
      <Button
        type='button'
        variant='ghost'
        size='icon'
        onClick={copy}
        aria-label={`Copy ${label}`}
        className='absolute top-1.5 right-1.5 size-7 text-muted-foreground hover:text-foreground'
      >
        {copied ? <Check className='size-3.5' /> : <Copy className='size-3.5' />}
      </Button>
    </div>
  );
}
