import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import { Cancel01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import * as stylex from "@stylexjs/stylex";
import type { ReactNode } from "react";

import { colors, fontSizes, lineHeights, radii, space } from "../../tokens.stylex.ts";

const sm = "@media (min-width: 40rem)";

const fadeIn = stylex.keyframes({ from: { opacity: 0 } });
const zoomIn = stylex.keyframes({ from: { opacity: 0, transform: "scale(0.95)" } });

const styles = stylex.create({
  backdrop: {
    position: "fixed",
    inset: 0,
    zIndex: 50,
    backgroundColor: "rgb(0 0 0 / 0.5)",
    backdropFilter: "blur(4px)",
    animationName: fadeIn,
    animationDuration: "200ms",
  },
  popup: {
    position: "fixed",
    top: "50%",
    left: "50%",
    zIndex: 50,
    translate: "-50% -50%",
    display: "grid",
    gap: "1.5rem",
    width: "100%",
    maxWidth: { default: "calc(100% - 2rem)", [sm]: "32rem" },
    maxHeight: "calc(100dvh - 2rem)",
    overflowY: "auto",
    padding: "clamp(1.5rem, 4vw, 2rem)",
    borderWidth: "1px",
    borderStyle: "solid",
    borderColor: colors.border,
    borderRadius: radii.xl,
    backgroundColor: colors.card,
    boxShadow: "0 24px 80px oklch(0.12 0.025 175 / 0.2)",
    outlineStyle: "none",
    animationName: zoomIn,
    animationDuration: "200ms",
  },
  header: { display: "flex", flexDirection: "column", gap: space.s2 },
  title: { fontSize: fontSizes.lg, lineHeight: 1, fontWeight: 600 },
  description: { fontSize: fontSizes.sm, lineHeight: lineHeights.sm, color: colors.mutedForeground },
  close: {
    position: "absolute",
    top: space.s4,
    right: space.s4,
    borderRadius: radii.xs,
    opacity: { default: 0.7, ":hover": 1 },
    transitionProperty: "opacity",
    transitionDuration: "150ms",
    boxShadow: { default: null, ":focus": `0 0 0 2px ${colors.ring}` },
    outline: { default: null, ":focus": "2px solid transparent" },
    outlineOffset: { default: null, ":focus": "2px" },
  },
  closeIcon: { width: space.s4, height: space.s4 },
  footer: {
    display: "flex",
    flexDirection: { default: "column-reverse", [sm]: "row" },
    justifyContent: { default: null, [sm]: "flex-end" },
    gap: space.s2,
  },
});

/** A dialog that is open while mounted; render it conditionally and unmount it from `onClose`. */
export function Dialog({
  title,
  description,
  onClose,
  children,
}: {
  title: string;
  description?: ReactNode;
  onClose: () => void;
  children: ReactNode;
}) {
  return (
    <BaseDialog.Root open onOpenChange={(open) => !open && onClose()}>
      <BaseDialog.Portal>
        <BaseDialog.Backdrop {...stylex.props(styles.backdrop)} />
        <BaseDialog.Popup {...stylex.props(styles.popup)}>
          <div {...stylex.props(styles.header)}>
            <BaseDialog.Title {...stylex.props(styles.title)}>{title}</BaseDialog.Title>
            {description && (
              <BaseDialog.Description {...stylex.props(styles.description)}>{description}</BaseDialog.Description>
            )}
          </div>
          {children}
          <BaseDialog.Close aria-label="Close" {...stylex.props(styles.close)}>
            <HugeiconsIcon icon={Cancel01Icon} {...stylex.props(styles.closeIcon)} />
          </BaseDialog.Close>
        </BaseDialog.Popup>
      </BaseDialog.Portal>
    </BaseDialog.Root>
  );
}

export function DialogFooter({ children }: { children: ReactNode }) {
  return <div {...stylex.props(styles.footer)}>{children}</div>;
}
