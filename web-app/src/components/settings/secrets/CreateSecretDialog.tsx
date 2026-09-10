import type React from "react";
import { useEffect, useState } from "react";
import { CopyFromWorkspace } from "@/components/settings/secrets/CopyFromWorkspace";
import { SecretInput } from "@/components/ui/SecretInput";
import { Button } from "@/components/ui/shadcn/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/shadcn/dialog";
import { FieldError } from "@/components/ui/shadcn/field";
import { Input } from "@/components/ui/shadcn/input";
import { Label } from "@/components/ui/shadcn/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue
} from "@/components/ui/shadcn/select";
import { Spinner } from "@/components/ui/shadcn/spinner";
import { Textarea } from "@/components/ui/shadcn/textarea";
import { useSetWorkspaceAppSecret } from "@/hooks/api/customApps/useAppSecrets";
import { useCustomApps } from "@/hooks/api/customApps/useCustomApps";
import { useCreateSecret } from "@/hooks/api/secrets/useSecretMutations";
import useCurrentProjectBranch from "@/hooks/useCurrentProjectBranch";
import { validateSecretName } from "@/libs/utils";
import type { CreateSecretRequest, CreateSecretResponse, SecretFormData } from "@/types/secret";

/** Sentinel for "not an app" — an empty string would collide with a cleared
 *  Select, and app ids are uuids so no real value can equal this. */
const WORKSPACE_SCOPE = "workspace";

interface CreateSecretDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSecretCreated: (secret: CreateSecretResponse) => void;
  initialName?: string;
}

export const CreateSecretDialog: React.FC<CreateSecretDialogProps> = ({
  open,
  onOpenChange,
  onSecretCreated,
  initialName
}) => {
  const createSecretMutation = useCreateSecret();

  // Scope: the workspace (an ordinary project secret), or one custom app (stored
  // as `apps/<app_id>/<KEY>`, which is the namespace that app's functions read
  // as `ctx.env.KEY`). Two write paths because they genuinely are two — the
  // project endpoint rejects the `/` an app-scoped name needs.
  const { project } = useCurrentProjectBranch();
  const { data: apps = [] } = useCustomApps(project.id);
  const [scope, setScope] = useState<string>(WORKSPACE_SCOPE);
  const setAppSecret = useSetWorkspaceAppSecret(project.id);
  const forApp = scope !== WORKSPACE_SCOPE;
  const pending = createSecretMutation.isPending || setAppSecret.isPending;
  const [formData, setFormData] = useState<SecretFormData>({
    name: initialName ?? "",
    value: "",
    description: ""
  });

  const [errors, setErrors] = useState<{ [key: string]: string }>({});

  // Sync name when dialog opens with a new initialName (e.g. switching override target)
  useEffect(() => {
    if (open) {
      setFormData({ name: initialName ?? "", value: "", description: "" });
      setScope(WORKSPACE_SCOPE);
      setErrors({});
    }
  }, [open, initialName]);

  const validateForm = (): boolean => {
    const newErrors: { [key: string]: string } = {};

    // Use the utility function for name validation
    const nameValidation = validateSecretName(formData.name);
    if (!nameValidation.isValid) {
      newErrors.name = nameValidation.error!;
    }

    if (!formData.value.trim()) {
      newErrors.value = "Secret value is required";
    }

    setErrors(newErrors);
    return Object.keys(newErrors).length === 0;
  };

  const handleCreateSecret = async () => {
    if (!validateForm()) {
      return;
    }

    try {
      if (forApp) {
        await setAppSecret.mutateAsync({
          appId: scope,
          key: formData.name.trim(),
          value: formData.value
        });
        // The app path has no create-response to hand back — the caller only
        // uses it to close and toast, and the list refetch is the real result.
        onSecretCreated({} as CreateSecretResponse);
        setFormData({ name: "", value: "", description: "" });
        setErrors({});
        return;
      }

      const request: CreateSecretRequest = {
        name: formData.name.trim(),
        value: formData.value,
        description: formData.description?.trim() || undefined
      };

      const response = await createSecretMutation.mutateAsync(request);
      onSecretCreated(response);

      // Reset form
      setFormData({
        name: "",
        value: "",
        description: ""
      });
      setErrors({});
    } catch (error) {
      console.error("Failed to create secret:", error);
      // Error toast is handled in the mutation hook
    }
  };

  const handleCancel = () => {
    setFormData({
      name: "",
      value: "",
      description: ""
    });
    setErrors({});
    onOpenChange(false);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className='sm:max-w-[425px]'>
        <DialogHeader>
          <DialogTitle>Create New Secret</DialogTitle>
          <DialogDescription>Store a new secret value securely.</DialogDescription>
        </DialogHeader>

        <div className='grid gap-4 py-4'>
          {apps.length > 0 && (
            <div className='grid gap-2'>
              <Label htmlFor='scope'>Available to</Label>
              <Select value={scope} onValueChange={setScope}>
                <SelectTrigger id='scope'>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={WORKSPACE_SCOPE}>This workspace</SelectItem>
                  {apps.map((app) => (
                    <SelectItem key={app.id} value={app.id}>
                      {app.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              {forApp && (
                <p className='text-muted-foreground text-xs'>
                  Only this app can read it, as <code>ctx.env.{formData.name.trim() || "KEY"}</code>
                  .
                </p>
              )}
            </div>
          )}

          <div className='grid gap-2'>
            <Label htmlFor='name'>Name *</Label>
            <Input
              id='name'
              placeholder='e.g., DATABASE_PASSWORD, API_KEY'
              value={formData.name}
              onChange={(e) => setFormData({ ...formData, name: e.target.value })}
              className={errors.name ? "border-destructive" : ""}
            />
            {errors.name && <FieldError>{errors.name}</FieldError>}
          </div>

          <div className='grid gap-2'>
            <Label htmlFor='value'>Value *</Label>
            <SecretInput
              id='value'
              placeholder='Enter secret value'
              value={formData.value}
              onChange={(e) => setFormData({ ...formData, value: e.target.value })}
              className={errors.value ? "border-destructive" : ""}
            />
            {errors.value && <FieldError>{errors.value}</FieldError>}
            {/* Only for an app secret: a workspace secret cannot be copied onto
                itself, and duplicate names are rejected anyway. */}
            {forApp && (
              <CopyFromWorkspace
                name={formData.name}
                onCopy={(value) => setFormData((prev) => ({ ...prev, value }))}
              />
            )}
          </div>

          <div className={forApp ? "hidden" : "grid gap-2"}>
            <Label htmlFor='description'>Description</Label>
            <Textarea
              id='description'
              placeholder='Optional description of this secret'
              value={formData.description}
              onChange={(e) => setFormData({ ...formData, description: e.target.value })}
              rows={3}
            />
          </div>
        </div>

        <DialogFooter>
          <Button variant='outline' onClick={handleCancel}>
            Cancel
          </Button>
          <Button onClick={handleCreateSecret} disabled={pending}>
            {pending ? <Spinner /> : "Create"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
