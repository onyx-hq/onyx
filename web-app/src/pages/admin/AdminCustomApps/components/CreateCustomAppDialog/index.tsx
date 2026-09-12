import { AxiosError } from "axios";
import { AlertTriangle, ExternalLink, FolderPlus, HelpCircle, Link2, Sparkles } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { Controller, useForm } from "react-hook-form";
import { toast } from "sonner";
import { Button } from "@/components/ui/shadcn/button";
import { Combobox } from "@/components/ui/shadcn/combobox";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/shadcn/dialog";
import { FieldError } from "@/components/ui/shadcn/field";
import { Input } from "@/components/ui/shadcn/input";
import { Label } from "@/components/ui/shadcn/label";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/shadcn/tooltip";
import { useAuth } from "@/contexts/AuthContext";
import { useCreateApp } from "@/hooks/api/customApps/useCreateApp";
import { useProbe } from "@/hooks/api/customApps/useProbe";
import { useOrgs } from "@/hooks/api/organizations";
import { useAllWorkspaces } from "@/hooks/api/workspaces/useWorkspaces";
import { cn } from "@/libs/shadcn/utils";
import type { CreateAppRequest, CustomApp, ProbeResponse } from "@/types/apps";
import { resolveBundleUrl } from "../../resolveBundleUrl";
import { FolderPicker } from "./FolderPicker";
import type { LinkPlan } from "./LinkPlanSummary";
import { LinkPlanSummary, summariseLinkPlan } from "./LinkPlanSummary";
import { TemplatePicker } from "./TemplatePicker";
import { WarningText } from "./WarningText";

type Props = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
};

/**
 * What the operator wants to do — three intents that map onto the
 * underlying SourceSpec but stay readable.
 *
 *   link    → "I already have a built bundle in a folder on the oxy
 *              host; serve it as-is." No publish step — good for local
 *              iteration. (source: local)
 *   create  → "Register a new app." Local: oxy provisions a folder to
 *              build into. Cloud: just writes the app row — you ship
 *              builds afterwards with `oxyc publish` (there is no CI).
 *   other   → v0 / Vercel URL escape hatch, iframe-wrapped.
 *              Experimental — tucked behind the third option so the
 *              two-button common case stays clean.
 *
 * This shape replaces the previous "pick a source type" radio, which
 * confronted operators with a chicken-and-egg between bundle base
 * path and app slug before they had any way to know what slug oxy
 * would generate. The new intents commit on operator outcome first
 * and let oxy figure out the plumbing afterwards.
 */
type Intent = "link" | "create" | "other";

type FormValues = {
  name: string;
  org_id: string;
  project_id: string;
  branch: string;
  intent: Intent;
  /** Local folder path — only used when intent === "link". */
  linkPath: string;
  /** v0 URL — only used when intent === "other". */
  v0Url: string;
  /** Template to scaffold from — only used when intent === "create". */
  template_id: string;
};

export const CreateCustomAppDialog = ({ open, onOpenChange }: Props) => {
  const { isLocalMode } = useAuth();
  // The folder picker is always available when the admin guard passes —
  // auth is the gate, not serve mode or an env flag.
  const linkEnabled = true;
  const { mutateAsync, isPending } = useCreateApp();
  const { data: orgs, isLoading: orgsLoading } = useOrgs();
  const [result, setResult] = useState<CustomApp | null>(null);
  // Inline submit error surfaced from the server's structured response
  // body (`{ message }`). Distinct from toast errors because slug
  // conflicts and other 4xx cases are blocking — the operator can't
  // proceed until they fix it, so the message has to stay visible.
  const [submitError, setSubmitError] = useState<string | null>(null);

  const {
    register,
    handleSubmit,
    reset,
    control,
    watch,
    setValue,
    formState: { errors }
  } = useForm<FormValues>({
    defaultValues: {
      branch: "main",
      // Local mode → "Link existing" is the operator's daily flow
      // (they already have a built folder on disk and just want oxy
      // to serve it). Cloud → default to "Create new" (register the
      // row, then ship with `oxyc publish`).
      intent: isLocalMode ? "link" : "create",
      linkPath: "",
      v0Url: "",
      template_id: "vite"
    }
  });

  const intent = watch("intent");
  const selectedOrgId = watch("org_id");
  const linkPath = watch("linkPath");
  const formOrgId = watch("org_id");
  const formProjectId = watch("project_id");

  // Probe the picked folder for an `oxy-app.json` + baked base path.
  // Only fires for the Link flow with a populated path; the manifest's
  // slug (when present) becomes the authoritative slug on submit so
  // operators can't pick one that won't match the baked bundle.
  const probe = useProbe(linkPath, intent === "link");

  // When the probe resolves for the first time (fresh dialog), auto-prefill
  // org and project from manifest hints — ONLY when the form fields are
  // currently empty so we never clobber an operator's deliberate picks.
  useEffect(() => {
    if (intent !== "link" || !probe.data?.ok) return;
    const probeData = probe.data;

    // Prefill org from manifest_org_slug if the form's org is empty.
    if (!formOrgId && probeData.manifest_org_slug && orgs) {
      const matchedOrg = orgs.find((o) => o.slug === probeData.manifest_org_slug);
      if (matchedOrg) {
        setValue("org_id", matchedOrg.id, { shouldValidate: false, shouldDirty: false });
      }
    }

    // Prefill project from manifest_project_id if the form's project is empty.
    if (!formProjectId && probeData.manifest_project_id) {
      const isValidUuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(
        probeData.manifest_project_id.trim()
      );
      if (isValidUuid) {
        setValue("project_id", probeData.manifest_project_id, {
          shouldValidate: false,
          shouldDirty: false
        });
      }
    }
    // setValue is stable (react-hook-form guarantees this), so including it
    // does not cause extra re-renders. formOrgId/formProjectId are included so
    // the linter is satisfied; the guards inside (`if (!formOrgId ...)`) ensure
    // we never overwrite an operator's deliberate pick.
  }, [intent, probe.data, orgs, formOrgId, formProjectId, setValue]);

  // Derive the full link plan from the probe + org list + operator's picks.
  // Computed once here and shared between the summary display and the submit
  // gate so the two stay in sync without duplicating logic.
  const linkPlan = useMemo<LinkPlan | null>(() => {
    if (intent !== "link") return null;
    return summariseLinkPlan(probe.data ?? null, linkPath, orgs, formOrgId, formProjectId);
  }, [intent, probe.data, linkPath, orgs, formOrgId, formProjectId]);

  const onSubmit = async (data: FormValues) => {
    setSubmitError(null);

    if (data.intent === "link") {
      if (!linkPlan?.ready) {
        setSubmitError("Bundle manifest is incomplete or invalid — fix and re-link.");
        return;
      }
    }

    const req = buildRequest(data, isLocalMode, linkPlan);
    try {
      const created = await mutateAsync(req);
      setResult(created);
      // Surface server-side warnings (e.g. local path lacks index.html)
      // immediately so the operator catches misconfiguration before
      // they preview the app and stare at a blank iframe.
      for (const warning of created.warnings ?? []) {
        toast.warning(warning, { duration: 8000 });
      }
      reset();
    } catch (err) {
      setSubmitError(extractServerMessage(err) ?? "Failed to create app.");
    }
  };

  const close = (v: boolean) => {
    onOpenChange(v);
    if (!v) {
      setResult(null);
      setSubmitError(null);
    }
  };

  return (
    <Dialog open={open} onOpenChange={close}>
      {/* max-h-[85vh] + flex column keeps the dialog inside the viewport
          regardless of how long the body grows (template picker +
          preview iframe in particular). The form itself becomes the
          scroll container so header + footer stay anchored.
          overflow-hidden + min-w-0 still clamps long absolute paths
          horizontally — long bundle paths in the FolderPicker
          breadcrumb shouldn't drag the modal past max-w-xl. */}
      <DialogContent className='flex max-h-[85vh] max-w-xl flex-col overflow-hidden'>
        <DialogHeader>
          <DialogTitle>{result ? "App created" : "Add custom app"}</DialogTitle>
        </DialogHeader>

        {!result && (
          <form
            onSubmit={handleSubmit(onSubmit)}
            className='flex min-h-0 min-w-0 flex-1 flex-col gap-4'
          >
            {/* Body scrolls so the footer stays anchored at the bottom.
                pr-1 prevents the scrollbar from overlapping focus rings. */}
            <div className='flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto pr-1'>
              <IntentPicker
                intent={intent}
                onChange={(next) => setValue("intent", next)}
                linkEnabled={linkEnabled}
                isLocalMode={isLocalMode}
              />

              <IntentDetails
                intent={intent}
                isLocalMode={isLocalMode}
                linkPath={linkPath}
                onLinkPathChange={(p) =>
                  setValue("linkPath", p, { shouldValidate: true, shouldDirty: true })
                }
                probe={probe.data ?? null}
                orgs={orgs}
                orgsLoading={orgsLoading}
                selectedOrgId={selectedOrgId}
                register={register}
                errors={errors}
                templateId={watch("template_id")}
                onTemplateChange={(id) => setValue("template_id", id, { shouldDirty: true })}
              />

              {/* Link plan summary — shown only in the link flow once a path
                is chosen. Reflects resolved identity from manifest + operator
                picks. */}
              {intent === "link" && linkPath && linkPlan && <LinkPlanSummary plan={linkPlan} />}

              <div className='border-border border-t pt-3' />

              {/* Name — only shown for Create / Other flows. In the link flow
                the name comes from the manifest (or folder basename). */}
              {intent !== "link" && (
                <div className='flex flex-col gap-1.5'>
                  <Label htmlFor='app-name'>Name</Label>
                  <Input
                    id='app-name'
                    placeholder='Store Pulse'
                    {...register("name", { required: "Required" })}
                  />
                  {errors.name && <FieldError>{errors.name.message}</FieldError>}
                </div>
              )}

              {/* Org / Project — shown for all intents. In the link flow the
                operator's picks are the source of truth; manifest hints
                prefill on first probe when the form is empty. */}
              <OrgPicker
                control={control}
                orgs={orgs}
                isLoading={orgsLoading}
                error={errors.org_id}
              />

              <ProjectPicker control={control} orgId={selectedOrgId} error={errors.project_id} />

              {submitError && (
                <div
                  role='alert'
                  className='flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/10 p-3 text-destructive text-xs'
                >
                  <AlertTriangle className='mt-0.5 size-4 shrink-0' />
                  <span className='min-w-0 flex-1 break-words'>{submitError}</span>
                </div>
              )}
            </div>
            <DialogFooter>
              <Button
                type='submit'
                disabled={
                  isPending ||
                  (intent === "link" && !linkPath) ||
                  (intent === "link" && probe.data != null && !probe.data.ok) ||
                  (intent === "link" &&
                    probe.data?.ok === true &&
                    linkPlan != null &&
                    !linkPlan.ready)
                }
              >
                {isPending ? "Creating…" : "Create"}
              </Button>
            </DialogFooter>
          </form>
        )}

        {result && <CreatedSummary app={result} onDone={() => close(false)} />}
      </DialogContent>
    </Dialog>
  );
};

/**
 * Translate intent + form values into the wire request. Centralised so
 * the submit handler stays readable and the intent → SourceSpec
 * mapping lives in one inspectable place.
 *
 * For the link flow, all identity (name, org_id, project_id, slug,
 * branch) is pulled from the resolved LinkPlan — FormValues name/org/
 * project are unused in this branch.
 */
const buildRequest = (
  data: FormValues,
  isLocalMode: boolean,
  plan: LinkPlan | null
): CreateAppRequest => {
  if (data.intent === "link") {
    if (!plan?.ready) {
      // Guard: onSubmit already checks this; this throw is unreachable.
      throw new Error("link plan required and must be ready");
    }
    return {
      name: plan.name,
      org_id: plan.orgId,
      project_id: plan.projectId,
      branch: plan.branch,
      slug: plan.slug,
      source: { type: "local", path: data.linkPath.trim() }
    };
  }

  const base: CreateAppRequest = {
    name: data.name,
    org_id: data.org_id,
    project_id: data.project_id,
    branch: data.branch || undefined
  };

  if (data.intent === "other") {
    return {
      ...base,
      source: { type: "v0", url: data.v0Url.trim() }
    };
  }

  // Create-new: branch on deployment mode.
  // Local → oxy mkdirs under $OXY_STATE_DIR and bakes the path back
  // into the row (build into it, or `oxyc publish --env local`).
  // Cloud → just register the app row; bytes arrive later via
  // `oxyc publish`. No scaffold PR / CI — that pipeline is gone.
  if (isLocalMode) {
    return {
      ...base,
      source: { type: "local", path: "" },
      provision_local_source: true,
      template_id: data.template_id || undefined
    };
  }
  return {
    ...base,
    source: { type: "s3" }
  };
};

type IntentPickerProps = {
  intent: Intent;
  onChange: (next: Intent) => void;
  /** True when the server permits linking against an on-disk folder
   *  (always in local mode; cloud mode needs the opt-in env var). */
  linkEnabled: boolean;
  /** Deployment mode — drives the "Create new" copy (provision a folder
   *  vs. open a scaffold PR). Independent of linkEnabled because a cloud
   *  server with the opt-in flag still uses the PR flow for Create. */
  isLocalMode: boolean;
};

const IntentPicker = ({ intent, onChange, linkEnabled, isLocalMode }: IntentPickerProps) => (
  <div className='flex flex-col gap-2'>
    <div className='grid grid-cols-3 gap-2'>
      {linkEnabled && (
        <IntentCard
          active={intent === "link"}
          onClick={() => onChange("link")}
          icon={<Link2 className='size-4' />}
          title='Link existing'
          description='Serve a built folder on disk as-is. No publish step.'
        />
      )}
      <IntentCard
        active={intent === "create"}
        onClick={() => onChange("create")}
        icon={<FolderPlus className='size-4' />}
        title='Create new'
        description={
          isLocalMode
            ? "Oxy provisions a folder; build in, or oxyc publish."
            : "Register the app, then ship with oxyc publish."
        }
      />
      <IntentCard
        active={intent === "other"}
        onClick={() => onChange("other")}
        icon={<Sparkles className='size-4' />}
        title='Other'
        description='Wrap a deployed Vercel app URL in an iframe (experimental).'
      />
    </div>
    <p className='text-muted-foreground text-xs leading-snug'>
      <strong className='font-medium text-foreground'>Link existing</strong> serves a folder on the
      oxy host directly (local iteration).{" "}
      <strong className='font-medium text-foreground'>Create new</strong> registers an app you ship
      versioned builds to with <code className='font-mono'>oxyc publish</code>.
    </p>
  </div>
);

type IntentCardProps = {
  active: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  title: string;
  description: string;
};

const IntentCard = ({ active, onClick, icon, title, description }: IntentCardProps) => (
  <button
    type='button'
    onClick={onClick}
    data-active={active}
    className={cn(
      "flex flex-col items-start gap-1.5 rounded-md border border-border p-3 text-left transition-colors",
      "hover:border-primary/40",
      "data-[active=true]:border-primary data-[active=true]:bg-primary/5"
    )}
  >
    <span
      className={cn(
        "flex size-7 items-center justify-center rounded-full",
        active ? "bg-primary text-primary-foreground" : "bg-muted text-muted-foreground"
      )}
    >
      {icon}
    </span>
    <span className='font-medium text-xs'>{title}</span>
    <span className='text-muted-foreground text-xs leading-snug'>{description}</span>
  </button>
);

type IntentDetailsProps = {
  intent: Intent;
  isLocalMode: boolean;
  linkPath: string;
  onLinkPathChange: (p: string) => void;
  probe: ProbeResponse | null;
  /** Threaded into BundleProbeSummary so it can validate the
   *  manifest's orgSlug claim against what this deployment has. */
  orgs: { id: string; slug: string }[] | undefined;
  orgsLoading: boolean;
  selectedOrgId: string | undefined;
  register: ReturnType<typeof useForm<FormValues>>["register"];
  errors: ReturnType<typeof useForm<FormValues>>["formState"]["errors"];
  /** Template selection — only used when intent === "create". */
  templateId: string;
  onTemplateChange: (id: string) => void;
};

const IntentDetails = ({
  intent,
  isLocalMode,
  linkPath,
  onLinkPathChange,
  probe,
  orgs,
  orgsLoading,
  selectedOrgId,
  register,
  errors,
  templateId,
  onTemplateChange
}: IntentDetailsProps) => {
  if (intent === "link") {
    return (
      <div className='flex flex-col gap-1.5'>
        <Label>Bundle folder</Label>
        <FolderPicker value={linkPath} onChange={onLinkPathChange} />
        <BundleProbeSummary
          probe={probe}
          linkPath={linkPath}
          orgs={orgs}
          orgsLoading={orgsLoading}
          selectedOrgId={selectedOrgId}
        />
      </div>
    );
  }

  if (intent === "create") {
    return (
      <div className='flex flex-col gap-3'>
        {isLocalMode ? (
          <>
            <div className='flex flex-col gap-1.5'>
              <Label>Start from</Label>
              <TemplatePicker value={templateId} onChange={onTemplateChange} />
            </div>
            <div className='rounded-md border border-border bg-muted/40 p-3 text-xs'>
              <p className='font-medium'>Oxy will create a folder for you.</p>
              <p className='mt-1 text-muted-foreground text-xs'>
                Path appears in the Settings tab after creation. Build into it, or ship a versioned
                build with <code className='font-mono'>oxyc publish --env local</code>.
              </p>
            </div>
          </>
        ) : (
          <div className='rounded-md border border-border bg-muted/40 p-3 text-xs'>
            <p className='font-medium'>Oxy registers the app row.</p>
            <p className='mt-1 text-muted-foreground text-xs'>
              No bundle is uploaded here. Ship builds from your app directory with{" "}
              <code className='font-mono'>oxyc publish</code> — the commands appear after you create
              it. There's no CI.
            </p>
          </div>
        )}
      </div>
    );
  }

  // intent === "other" → Vercel deployment URL
  return (
    <div className='flex flex-col gap-1.5'>
      <Label htmlFor='app-v0-url'>Vercel deployment URL</Label>
      <Input
        id='app-v0-url'
        type='url'
        placeholder='https://your-app.vercel.app/'
        {...register("v0Url", {
          required: "Required",
          validate: (v) => /^https?:\/\//i.test(v.trim()) || "Must be an http(s) URL"
        })}
      />
      {errors.v0Url && <FieldError>{errors.v0Url.message}</FieldError>}
      <div className='flex items-center gap-1.5 text-muted-foreground text-xs'>
        <span>Iframe-wrapped URL. No data integration with this project.</span>
        <Tooltip>
          <TooltipTrigger asChild>
            <HelpCircle className='h-3 w-3 shrink-0' />
          </TooltipTrigger>
          <TooltipContent className='max-w-xs'>
            v0-source apps render the linked URL in an iframe. Oxy doesn't inject session auth or
            expose <code>useQuery</code> to v0 bundles — the bundle's data layer must be
            self-contained. For full data integration, use <strong>Create new</strong> instead.
          </TooltipContent>
        </Tooltip>
      </div>
    </div>
  );
};

/**
 * Identity card shown under the FolderPicker when the link path is set.
 * Surfaces the bundle's self-declared name + slug from `oxy-app.json`
 * (when present) and the baked base path detected in `index.html`.
 *
 * Three states by priority:
 *   1. **Manifest slug present** → green confirmation. The slug is
 *      locked to the manifest; create succeeds without surprises.
 *   2. **No manifest but baked path detected** → neutral hint with the
 *      slug we'll use (parsed from the baked path).
 *   3. **Mismatched / no index.html** → amber warning that explains
 *      what'll happen (stuck "Loading…") and what to do about it.
 */
const BundleProbeSummary = ({
  probe,
  linkPath,
  orgs,
  orgsLoading,
  selectedOrgId
}: {
  probe: ProbeResponse | null;
  linkPath: string;
  /** Orgs the caller can see — used to validate the manifest's
   *  declared orgSlug exists in this deployment. */
  orgs: { id: string; slug: string }[] | undefined;
  /** True while `useOrgs` is fetching — suppresses the "no such org"
   *  warning during the brief window before the list arrives. */
  orgsLoading: boolean;
  /** The currently-picked org_id, so we can warn when the operator
   *  overrode the manifest's declared org (legitimate, but worth
   *  flagging — they may have meant to use the declared one). */
  selectedOrgId: string | undefined;
}) => {
  if (!linkPath || !probe) return null;

  // Probe rejected the manifest — show a destructive alert listing all
  // warnings and suppress the normal identity-chip entirely. The submit
  // button is also disabled at this point so the operator can't proceed.
  if (!probe.ok && probe.warnings.length > 0) {
    return (
      <div
        role='alert'
        className='flex flex-col gap-1.5 rounded-md border border-destructive/40 bg-destructive/10 p-3 text-destructive text-xs'
      >
        <div className='flex items-center gap-2 font-medium'>
          <AlertTriangle className='size-4 shrink-0' />
          <span>Bundle rejected — fix the issues below before linking:</span>
        </div>
        <ul className='flex flex-col gap-1 pl-6 text-xs'>
          {probe.warnings.map((w) => (
            <li key={w} className='list-disc'>
              <WarningText text={w} />
            </li>
          ))}
        </ul>
      </div>
    );
  }

  const bakedSlug = slugFromBakedBasePath(probe.baked_base_path);
  const slug = probe.manifest_slug ?? bakedSlug;

  // Org-validation state for the manifest's declared orgSlug:
  //   - "match": declared org exists, currently selected.
  //   - "exists-but-overridden": declared org exists, operator
  //     picked a different one (legitimate; surface as info).
  //   - "missing": declared org doesn't exist in this deployment.
  //     Most common cause: bundle imported from another env (prod
  //     manifest declares `pokehouse`, your local oxy doesn't have
  //     a `pokehouse` org).
  //   - null: manifest doesn't declare an orgSlug.
  let orgState: "match" | "exists-but-overridden" | "missing" | null = null;
  if (probe.manifest_org_slug && !orgsLoading && orgs) {
    const match = orgs.find((o) => o.slug === probe.manifest_org_slug);
    if (!match) {
      orgState = "missing";
    } else if (selectedOrgId && selectedOrgId !== match.id) {
      orgState = "exists-but-overridden";
    } else {
      orgState = "match";
    }
  }

  // No index.html in the exact folder picked. The probe is strict — it
  // doesn't peek into `out/` or `dist/` subdirs on its own — so the
  // operator either hasn't built yet or picked the project root and
  // needs to dive into the build-output folder.
  if (!probe.has_index_html) {
    return (
      <div className='flex items-start gap-2 rounded-md border border-border border-dashed bg-muted/30 p-2 text-xs'>
        <AlertTriangle className='size-3.5 shrink-0 text-muted-foreground' />
        <span className='text-muted-foreground'>
          No <code className='font-mono'>index.html</code> in this folder. If you build into{" "}
          <code className='font-mono'>out/</code> or <code className='font-mono'>dist/</code>,
          navigate into that subfolder and pick it directly.
        </span>
      </div>
    );
  }

  return (
    <div className='flex flex-col gap-1 rounded-md border border-border bg-muted/30 p-2 text-xs'>
      {probe.manifest_slug && (
        <p>
          <span className='font-medium'>Slug:</span>{" "}
          <code className='break-all rounded bg-primary/10 px-1 py-0.5 font-mono text-primary'>
            {probe.manifest_slug}
          </code>{" "}
          <span className='text-muted-foreground'>
            (from <code className='font-mono'>oxy-app.json</code>)
          </span>
        </p>
      )}
      {!probe.manifest_slug && bakedSlug && (
        <p>
          <span className='font-medium'>Slug:</span>{" "}
          <code className='break-all rounded bg-muted px-1 py-0.5 font-mono'>{bakedSlug}</code>{" "}
          <span className='text-muted-foreground'>(detected from baked base path)</span>
        </p>
      )}
      {!slug && (
        <p className='flex items-start gap-1.5 text-amber-700 dark:text-amber-500'>
          <AlertTriangle className='mt-0.5 size-3.5 shrink-0' />
          <span>
            No slug in <code className='font-mono'>oxy-app.json</code> and no baked base path found
            in <code className='font-mono'>index.html</code>. We'll derive the slug from the app
            name — verify it matches your bundle's{" "}
            <code className='font-mono'>OXY_APP_BASE_PATH</code> or the app will sit at "Loading…"
            forever.
          </span>
        </p>
      )}
      {probe.baked_base_path && probe.manifest_slug && bakedSlug !== probe.manifest_slug && (
        <p className='flex items-start gap-1.5 text-amber-700 dark:text-amber-500'>
          <AlertTriangle className='mt-0.5 size-3.5 shrink-0' />
          <span>
            Manifest slug <code className='font-mono'>{probe.manifest_slug}</code> doesn't match the
            baked base path <code className='font-mono'>{probe.baked_base_path}</code>. Rebuild with{" "}
            <code className='font-mono'>
              OXY_APP_BASE_PATH=/customer-apps/&lt;org&gt;/{probe.manifest_slug}/
            </code>
            .
          </span>
        </p>
      )}
      {orgState === "missing" && (
        <p className='flex items-start gap-1.5 text-amber-700 dark:text-amber-500'>
          <AlertTriangle className='mt-0.5 size-3.5 shrink-0' />
          <span>
            Manifest declares{" "}
            <code className='font-mono'>orgSlug: "{probe.manifest_org_slug}"</code> but no such org
            exists in this deployment.
          </span>
        </p>
      )}
      {orgState === "exists-but-overridden" && (
        <p className='flex items-start gap-1.5 text-muted-foreground'>
          <AlertTriangle className='mt-0.5 size-3.5 shrink-0' />
          <span>
            Manifest declares{" "}
            <code className='font-mono'>orgSlug: "{probe.manifest_org_slug}"</code> but you've
            picked a different org. That's fine — the manifest claim is a hint, not a gate. Remember
            to rebuild the bundle with <code className='font-mono'>OXY_APP_BASE_PATH</code> matching
            your chosen org or the served URL will 404 every asset.
          </span>
        </p>
      )}
      {orgState === "match" && (
        <p className='text-muted-foreground'>
          <span className='font-medium text-foreground'>Org:</span>{" "}
          <code className='rounded bg-primary/10 px-1 py-0.5 font-mono text-primary'>
            {probe.manifest_org_slug}
          </code>{" "}
          (from <code className='font-mono'>oxy-app.json</code>, prefilled)
        </p>
      )}
      {/* Kit nudge — only shown when we have positive evidence
          (package.json exists, plugin absent). `null` (couldn't
          determine — likely picked the `out/` dir without a sibling
          source folder) is silent; many bundles will be hand-rolled
          forever and we don't want to badger their operators. */}
      {probe.uses_oxy_kit === false && (
        <p className='text-muted-foreground'>
          <span className='font-medium text-foreground'>Tip:</span> Bundle isn't using{" "}
          <code className='font-mono'>@oxy-hq/vite-plugin</code>. The kit handles base path,
          manifest validation, and dev shim in one line — drop in{" "}
          <code className='font-mono'>oxyApp()</code> next to your{" "}
          <code className='font-mono'>react()</code> plugin. See customer-apps.md § "Oxy App Kit".
        </p>
      )}
      {probe.uses_oxy_kit === true && (
        <p className='text-muted-foreground'>
          <span className='font-medium text-foreground'>Kit:</span>{" "}
          <code className='rounded bg-primary/10 px-1 py-0.5 font-mono text-primary'>
            @oxy-hq/vite-plugin
          </code>{" "}
          detected.
        </p>
      )}
      {probe.manifest_project_id && (
        <p className='text-muted-foreground'>
          <span className='font-medium text-foreground'>Project:</span>{" "}
          <code className='rounded bg-muted px-1 py-0.5 font-mono text-xs'>
            {probe.manifest_project_id}
          </code>{" "}
          (from <code className='font-mono'>oxy-app.json</code>)
        </p>
      )}
    </div>
  );
};

/** RFC 4122 shape check for the manual-UUID escape hatches below. */
const isUuidShape = (s: string | undefined): boolean =>
  !!s && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(s.trim());

/** Pull the human message out of an axios error response. The server
 *  returns `{ message: "..." }` for 4xx/5xx on the admin apps surface
 *  (see backend's `ErrorBody`); falls back to the JS error message
 *  for unexpected throws. Returns null if nothing useful is available
 *  so the caller can default to a generic copy. */
const extractServerMessage = (err: unknown): string | null => {
  if (err instanceof AxiosError) {
    const data = err.response?.data;
    if (data && typeof data === "object" && "message" in data) {
      const msg = (data as { message?: unknown }).message;
      if (typeof msg === "string" && msg.trim()) return msg;
    }
  }
  if (err instanceof Error && err.message) return err.message;
  return null;
};

/** Extract the `<slug>` segment from a baked `/customer-apps/<org>/<slug>/`
 *  prefix detected in the bundle's `index.html`. Used as a fallback slug
 *  source when the manifest doesn't declare one. */
const slugFromBakedBasePath = (baked: string | null | undefined): string | null => {
  if (!baked) return null;
  const m = baked.match(/^\/customer-apps\/[^/]+\/([^/]+)\/?$/);
  return m?.[1] ?? null;
};

type OrgPickerProps = {
  control: import("react-hook-form").Control<FormValues>;
  orgs: { id: string; name: string; slug: string }[] | undefined;
  isLoading: boolean;
  error: import("react-hook-form").FieldError | undefined;
};

/**
 * Searchable org picker with a "paste a UUID instead" escape hatch.
 * Global Admins can be registering apps for orgs they're not a member
 * of — those won't surface in `useOrgs()` and the only option is
 * to paste the uuid.
 */
const OrgPicker = ({ control, orgs, isLoading, error }: OrgPickerProps) => {
  const [manual, setManual] = useState(false);
  const items = (orgs ?? []).map((org) => ({
    value: org.id,
    label: org.name,
    searchText: `${org.name} ${org.slug}`
  }));

  return (
    <div className='flex flex-col gap-1.5'>
      <Label htmlFor='app-org-id'>Organization</Label>
      <Controller
        name='org_id'
        control={control}
        rules={{
          required: "Required",
          validate: (v) => isUuidShape(v) || "Must be a valid UUID"
        }}
        render={({ field }) =>
          manual ? (
            <Input
              id='app-org-id'
              placeholder='xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx'
              value={field.value ?? ""}
              onChange={(e) => field.onChange(e.target.value)}
            />
          ) : (
            <Combobox
              items={items}
              value={field.value || undefined}
              onValueChange={field.onChange}
              placeholder={isLoading ? "Loading orgs…" : "Select an organization"}
              searchPlaceholder='Search by name or slug…'
              disabled={isLoading}
              renderItem={(item) => {
                const org = orgs?.find((o) => o.id === item.value);
                return (
                  <span className='flex flex-1 items-center justify-between'>
                    <span>{item.label}</span>
                    {org && <span className='text-muted-foreground text-xs'>{org.slug}</span>}
                  </span>
                );
              }}
            />
          )
        }
      />
      <button
        type='button'
        onClick={() => setManual(!manual)}
        className='self-start text-muted-foreground text-xs hover:text-foreground'
      >
        {manual ? "Pick from the list instead" : "Paste a UUID instead"}
      </button>
      {error && <FieldError>{error.message}</FieldError>}
    </div>
  );
};

type ProjectPickerProps = {
  control: import("react-hook-form").Control<FormValues>;
  orgId: string | undefined;
  error: import("react-hook-form").FieldError | undefined;
};

const ProjectPicker = ({ control, orgId, error }: ProjectPickerProps) => {
  const { data: workspaces, isLoading } = useAllWorkspaces(orgId);
  const [manual, setManual] = useState(false);
  const hasOrg = !!orgId;
  const items = (workspaces ?? []).map((ws) => ({
    value: ws.id,
    label: ws.name,
    searchText: `${ws.name} ${ws.id}`
  }));

  const placeholder = !hasOrg
    ? "Select an organization first"
    : isLoading
      ? "Loading workspaces…"
      : items.length === 0
        ? "No workspaces in this org — paste a UUID"
        : "Select a workspace";

  return (
    <div className='flex flex-col gap-1.5'>
      <Label htmlFor='app-project-id'>Project (workspace)</Label>
      <Controller
        name='project_id'
        control={control}
        rules={{
          required: "Required",
          validate: (v) => isUuidShape(v) || "Must be a valid UUID"
        }}
        render={({ field }) =>
          manual ? (
            <Input
              id='app-project-id'
              placeholder='xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx'
              value={field.value ?? ""}
              onChange={(e) => field.onChange(e.target.value)}
            />
          ) : (
            <Combobox
              items={items}
              value={field.value || undefined}
              onValueChange={field.onChange}
              placeholder={placeholder}
              searchPlaceholder='Search by workspace name…'
              disabled={!hasOrg || isLoading}
              renderItem={(item) => (
                <span className='flex flex-1 items-center justify-between'>
                  <span>{item.label}</span>
                  <span className='text-muted-foreground text-xs'>{item.value.slice(0, 8)}</span>
                </span>
              )}
            />
          )
        }
      />
      <button
        type='button'
        onClick={() => setManual(!manual)}
        className='self-start text-muted-foreground text-xs hover:text-foreground'
      >
        {manual ? "Pick from the list instead" : "Paste a UUID instead"}
      </button>
      {error && <FieldError>{error.message}</FieldError>}
    </div>
  );
};

const CreatedSummary = ({ app, onDone }: { app: CustomApp; onDone: () => void }) => (
  // min-w-0 throughout — long absolute paths and PR URLs would
  // otherwise widen the modal past max-w-xl. Inline `<code>` blocks
  // can't break on a slash, so we lean on `break-all` for them.
  <div className='flex min-w-0 flex-col gap-3 text-xs'>
    <div className='flex min-w-0 flex-col gap-1'>
      <SummaryRow label='ID'>
        <code className='break-all rounded bg-muted px-1 py-0.5 font-mono text-xs'>{app.id}</code>
      </SummaryRow>
      <SummaryRow label='URL'>
        <a
          href={resolveBundleUrl(app.url)}
          className='inline-flex items-center gap-1 break-all text-primary underline underline-offset-4'
          target='_blank'
          rel='noopener noreferrer'
        >
          {app.url} <ExternalLink className='size-3' />
        </a>
      </SummaryRow>
      <SummaryRow label='Source'>
        <code className='rounded bg-muted px-1 py-0.5 font-mono text-xs'>{app.source_type}</code>
      </SummaryRow>
      {app.source_type === "local" && (
        <SummaryRow label='Local path'>
          <code className='break-all rounded bg-muted px-1 py-0.5 font-mono text-xs'>
            {(app.source_config?.path as string | undefined) ?? "—"}
          </code>
        </SummaryRow>
      )}
      {app.bootstrap_pr_url && (
        <SummaryRow label='Scaffold PR'>
          <a
            href={app.bootstrap_pr_url}
            className='inline-flex items-center gap-1 break-all text-primary underline underline-offset-4'
            target='_blank'
            rel='noopener noreferrer'
          >
            {app.bootstrap_pr_url} <ExternalLink className='size-3' />
          </a>
        </SummaryRow>
      )}
    </div>

    <NextSteps app={app} />

    <DialogFooter>
      <Button onClick={onDone}>Done</Button>
    </DialogFooter>
  </div>
);

const SummaryRow = ({ label, children }: { label: string; children: React.ReactNode }) => (
  <p>
    <span className='font-medium'>{label}:</span> {children}
  </p>
);

const NextSteps = ({ app }: { app: CustomApp }) => {
  if (app.source_type === "v0") {
    return <p className='text-muted-foreground'>Open the URL above to preview.</p>;
  }
  if (app.source_type === "local") {
    const path = (app.source_config?.path as string | undefined) ?? "<path>";
    return (
      <div className='text-muted-foreground'>
        <p className='mb-1'>Build into the provisioned folder, then open the URL:</p>
        <pre className='overflow-x-auto rounded bg-muted px-2 py-1.5 font-mono text-foreground text-xs'>
          {`cd ${path}\npnpm install\npnpm build`}
        </pre>
      </div>
    );
  }
  return (
    <div className='flex flex-col gap-1.5'>
      <p className='font-medium'>
        Next: ship a build with <code className='font-mono'>oxyc publish</code>
      </p>
      <p className='text-muted-foreground text-xs'>
        The app is registered, so publish resolves the project automatically — no{" "}
        <code className='font-mono'>--project</code> needed.
      </p>
      <pre className='overflow-x-auto rounded bg-muted px-2 py-1.5 font-mono text-foreground text-xs'>
        {`# from your app dir — oxy-app.json: { "slug": "${app.slug}", "orgSlug": "${app.org_slug}" }
npm install -g @oxy-hq/cli               # once
oxyc login --env production
oxyc publish --env production            # → draft
oxyc publish --env production --promote  # → live`}
      </pre>
      <p className='text-muted-foreground text-xs'>
        No app code yet? Scaffold one:{" "}
        <code className='font-mono'>pnpm dlx create-oxy-app {app.slug} --template vite</code>
      </p>
    </div>
  );
};
