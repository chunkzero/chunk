import { useMutation } from "@tanstack/react-query";
import type { ReactNode } from "react";

import { errorMessage } from "../lib/client.ts";
import { ErrorText } from "./page.tsx";
import { Button } from "./ui/button.tsx";
import { Dialog, DialogFooter } from "./ui/dialog.tsx";

/** Runs `action` on confirm, then closes. Render it only while open, so state from a previous opening never lingers. */
export function ConfirmDialog({
  title,
  description,
  confirmLabel,
  destructive = false,
  disabled = false,
  action,
  onClose,
  children,
}: {
  title: string;
  description: ReactNode;
  confirmLabel: string;
  destructive?: boolean;
  disabled?: boolean;
  action: () => Promise<unknown>;
  onClose: () => void;
  children?: ReactNode;
}) {
  const mutation = useMutation({ mutationFn: action });
  return (
    <Dialog title={title} description={description} onClose={onClose}>
      <form
        className="space-y-6"
        onSubmit={(event) => {
          event.preventDefault();
          // Passed to mutate, onClose only runs while this dialog is mounted, never for a dialog opened after it.
          mutation.mutate(undefined, { onSuccess: onClose });
        }}
      >
        {children}
        <ErrorText error={mutation.error ? errorMessage(mutation.error) : undefined} />
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button
            type="submit"
            variant={destructive ? "destructive" : "default"}
            disabled={disabled || mutation.isPending}
          >
            {confirmLabel}
          </Button>
        </DialogFooter>
      </form>
    </Dialog>
  );
}
