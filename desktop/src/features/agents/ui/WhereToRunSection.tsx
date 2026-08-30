import { AlertTriangle } from "lucide-react";
import * as React from "react";

import { useBackendProvidersQuery } from "@/features/agents/hooks";
import {
  cloneFirstMateHome,
  pickFirstMateHome,
  probeBackendProvider,
  validateFirstMateHome,
} from "@/shared/api/tauri";
import { cn } from "@/shared/lib/cn";
import { Button } from "@/shared/ui/button";
import { Input } from "@/shared/ui/input";

import { ProviderConfigFields } from "./ProviderConfigFields";
import { PersonaDropdownField } from "./PersonaDropdownField";
import {
  applyProbeResult,
  emptyWhereToRunDraft,
  type WhereToRunDraft,
} from "./whereToRunIntent";

/** Optional remote-backend selector. Buzz shared compute is an LLM provider, not a run destination. */
export function WhereToRunSection({
  draft,
  isPending,
  onDraftChange,
}: {
  draft: WhereToRunDraft;
  isPending: boolean;
  onDraftChange: (next: WhereToRunDraft) => void;
}) {
  const backendProviders = useBackendProvidersQuery().data ?? [];
  const [probeError, setProbeError] = React.useState<string | null>(null);
  const [firstmateHomeStatus, setFirstmateHomeStatus] = React.useState<
    "idle" | "checking" | "valid" | "invalid"
  >("idle");
  const [firstmateHomeMessage, setFirstmateHomeMessage] = React.useState("");
  const [isCloningFirstmate, setIsCloningFirstmate] = React.useState(false);
  const runOnOptions = React.useMemo(
    () => [
      { label: "This computer", value: "local" },
      ...backendProviders.map((provider) => ({
        label: provider.id,
        value: provider.id,
      })),
    ],
    [backendProviders],
  );
  const isProviderMode = draft.runOn !== "local";
  const selectedBackendProvider = React.useMemo(
    () =>
      backendProviders.find((provider) => provider.id === draft.runOn) ?? null,
    [backendProviders, draft.runOn],
  );

  // Latest-state seam for probe resolution: an Effect Event always sees the
  // draft as it is *now*. Without this, the probe promise closes over the
  // draft from probe start, and anything typed while the probe was in flight
  // gets thrown away when it resolves (a second, subtler Typewriter Eraser).
  const applyProbe = React.useEffectEvent(
    (result: Awaited<ReturnType<typeof probeBackendProvider>>) => {
      onDraftChange(applyProbeResult(draft, result));
    },
  );

  // Probe once per provider *selection*, keyed on the provider's stable
  // path — never on the draft. Depending on the draft made every keystroke
  // refire the probe, and each resolution reset providerConfig to schema
  // defaults, which erased what the user was typing (the Typewriter Eraser)
  // and spawned the provider binary in a loop for as long as the dialog was
  // open. Keying on the path (not the provider object) also keeps a
  // providers-query refresh from reprobing an unchanged selection.
  const selectedBinaryPath = isProviderMode
    ? (selectedBackendProvider?.binaryPath ?? null)
    : null;
  React.useEffect(() => {
    if (!selectedBinaryPath || draft.probedProvider) {
      setProbeError(null);
      return;
    }
    let cancelled = false;
    setProbeError(null);
    void probeBackendProvider(selectedBinaryPath)
      .then((result) => {
        if (cancelled) return;
        applyProbe(result);
      })
      .catch((error: unknown) => {
        if (!cancelled) {
          setProbeError(error instanceof Error ? error.message : String(error));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [selectedBinaryPath, draft.probedProvider]);

  React.useEffect(() => {
    if (!draft.firstmate) {
      setFirstmateHomeStatus("idle");
      setFirstmateHomeMessage("");
      return;
    }
    const path = draft.firstmateHome.trim();
    if (!path) {
      setFirstmateHomeStatus("idle");
      setFirstmateHomeMessage("");
      if (draft.firstmateHomeValid) {
        onDraftChange({ ...draft, firstmateHomeValid: false });
      }
      return;
    }
    let cancelled = false;
    setFirstmateHomeStatus("checking");
    const timer = window.setTimeout(() => {
      void validateFirstMateHome(path).then(
        (canonical) => {
          if (cancelled) return;
          setFirstmateHomeStatus("valid");
          setFirstmateHomeMessage(`Validated ${canonical}`);
          if (!draft.firstmateHomeValid || canonical !== draft.firstmateHome) {
            onDraftChange({
              ...draft,
              firstmateHome: canonical,
              firstmateHomeValid: true,
            });
          }
        },
        (error: unknown) => {
          if (cancelled) return;
          setFirstmateHomeStatus("invalid");
          setFirstmateHomeMessage(
            error instanceof Error ? error.message : String(error),
          );
          if (draft.firstmateHomeValid) {
            onDraftChange({ ...draft, firstmateHomeValid: false });
          }
        },
      );
    }, 250);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [draft, onDraftChange]);

  return (
    <div className="space-y-4">
      <div className="space-y-1.5">
        <label className="text-sm font-medium" htmlFor="agent-run-on">
          Run on
        </label>
        <PersonaDropdownField
          disabled={isPending}
          id="agent-run-on"
          onValueChange={(runOn) =>
            onDraftChange({
              ...emptyWhereToRunDraft,
              runOn,
            })
          }
          options={runOnOptions}
          placeholder="Choose where to run"
          value={draft.runOn}
        />
      </div>

      {!isProviderMode ? (
        <div className="space-y-3 rounded-md border border-border/70 bg-muted/20 p-3">
          <label className="flex cursor-pointer items-start gap-2 text-sm">
            <input
              checked={draft.firstmate}
              disabled={isPending}
              onChange={(event) =>
                onDraftChange({
                  ...draft,
                  firstmate: event.target.checked,
                  firstmateHomeValid: event.target.checked
                    ? draft.firstmateHomeValid
                    : false,
                })
              }
              type="checkbox"
            />
            <span className="space-y-0.5">
              <span className="block font-medium">
                FirstMate orchestration home
              </span>
              <span className="block text-xs text-muted-foreground">
                One continuous primary session coordinates crewmates in its own
                workspace on the shared Herdr server.
              </span>
            </span>
          </label>
          {draft.firstmate ? (
            <div className="space-y-1.5 pl-6">
              <label
                className="text-xs font-medium"
                htmlFor="new-firstmate-home"
              >
                FirstMate home path
              </label>
              <div className="flex gap-2">
                <Input
                  aria-describedby="new-firstmate-home-status"
                  aria-invalid={firstmateHomeStatus === "invalid"}
                  disabled={isPending || isCloningFirstmate}
                  id="new-firstmate-home"
                  onChange={(event) =>
                    onDraftChange({
                      ...draft,
                      firstmateHome: event.target.value,
                      firstmateHomeValid: false,
                    })
                  }
                  placeholder="/absolute/path/to/fm-home"
                  value={draft.firstmateHome}
                />
                <Button
                  disabled={isPending || isCloningFirstmate}
                  onClick={() => {
                    void pickFirstMateHome().then((path) => {
                      if (path) {
                        onDraftChange({
                          ...draft,
                          firstmateHome: path,
                          firstmateHomeValid: false,
                        });
                      }
                    });
                  }}
                  type="button"
                  variant="outline"
                >
                  Choose folder
                </Button>
              </div>
              <p
                className={cn(
                  "text-xs",
                  firstmateHomeStatus === "invalid"
                    ? "text-destructive"
                    : "text-muted-foreground",
                )}
                id="new-firstmate-home-status"
              >
                {firstmateHomeStatus === "checking"
                  ? "Checking AGENTS.md, bin/, state/, and canonical path…"
                  : firstmateHomeMessage ||
                    "Buzz validates and stores the canonical fm-home path."}
              </p>
              {firstmateHomeStatus === "invalid" &&
              draft.firstmateHome.trim() ? (
                <div className="flex items-center justify-between gap-3 rounded-md border border-border bg-background p-2.5">
                  <p className="text-xs text-muted-foreground">
                    Not a FirstMate home yet. Clone the official FirstMate
                    repository into this empty folder?
                  </p>
                  <Button
                    className="shrink-0"
                    disabled={isPending || isCloningFirstmate}
                    onClick={() => {
                      setIsCloningFirstmate(true);
                      setFirstmateHomeStatus("checking");
                      setFirstmateHomeMessage("Cloning FirstMate…");
                      void cloneFirstMateHome(draft.firstmateHome.trim()).then(
                        (canonical) => {
                          setIsCloningFirstmate(false);
                          setFirstmateHomeStatus("valid");
                          setFirstmateHomeMessage(
                            `Cloned and validated ${canonical}`,
                          );
                          onDraftChange({
                            ...draft,
                            firstmateHome: canonical,
                            firstmateHomeValid: true,
                          });
                        },
                        (error: unknown) => {
                          setIsCloningFirstmate(false);
                          setFirstmateHomeStatus("invalid");
                          setFirstmateHomeMessage(
                            error instanceof Error
                              ? error.message
                              : String(error),
                          );
                        },
                      );
                    }}
                    size="sm"
                    type="button"
                    variant="outline"
                  >
                    {isCloningFirstmate ? "Cloning…" : "Clone FirstMate"}
                  </Button>
                </div>
              ) : null}
              <p className="text-xs text-muted-foreground">
                Local backend, one worker, agent-scoped continuity, supervisor,
                and Herdr workspace are configured automatically.
              </p>
            </div>
          ) : null}
        </div>
      ) : null}

      {isProviderMode && selectedBackendProvider ? (
        <div className="space-y-4">
          <div className="flex gap-3 rounded-2xl border border-warning/30 bg-warning-bg px-4 py-3">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-warning" />
            <p className="text-sm text-warning">
              This provider at{" "}
              <span className="font-mono font-medium">
                {selectedBackendProvider.binaryPath}
              </span>{" "}
              will receive your agent&apos;s private key. Only use providers
              from trusted sources.
            </p>
          </div>
          {probeError ? (
            <p className="rounded-2xl border border-destructive/30 bg-destructive/10 px-4 py-3 text-sm text-destructive">
              Could not probe provider: {probeError}
            </p>
          ) : null}
          {draft.probedProvider?.config_schema ? (
            <ProviderConfigFields
              config={draft.providerConfig}
              onChange={(providerConfig) =>
                onDraftChange({ ...draft, providerConfig })
              }
              schema={draft.probedProvider.config_schema}
            />
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
