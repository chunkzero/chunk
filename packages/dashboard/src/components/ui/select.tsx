import { Select as BaseSelect } from "@base-ui/react/select";
import { Tick02Icon, UnfoldMoreIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import * as stylex from "@stylexjs/stylex";

import { colors, fontSizes, lineHeights, radii, space } from "../../tokens.stylex.ts";
import { fieldStyles } from "./input.tsx";

const styles = stylex.create({
  trigger: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    gap: space.s2,
    height: "2.475rem",
    minHeight: "2.75rem",
    paddingInline: "1rem",
    textAlign: "left",
    cursor: "pointer",
  },
  indicator: { display: "flex", flexShrink: 0 },
  icon: { width: space.s4, height: space.s4, color: colors.mutedForeground },
  positioner: { zIndex: 50 },
  popup: {
    minWidth: "var(--anchor-width)",
    maxHeight: "var(--available-height)",
    overflowY: "auto",
    padding: space.s1,
    borderWidth: "1px",
    borderStyle: "solid",
    borderColor: colors.border,
    borderRadius: radii.md,
    backgroundColor: colors.card,
    boxShadow: "0 12px 40px oklch(0.12 0.025 175 / 0.15)",
    outlineStyle: "none",
  },
  item: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    gap: space.s2,
    paddingInline: space.s2,
    paddingBlock: space.s1_5,
    borderRadius: radii.sm,
    fontSize: fontSizes.sm,
    lineHeight: lineHeights.sm,
    cursor: "default",
    outlineStyle: "none",
    userSelect: "none",
  },
  highlighted: { backgroundColor: colors.accent, color: colors.accentForeground },
});

/** Picks one of `items` by its value. */
export function Select({
  id,
  value,
  onValueChange,
  items,
}: {
  id?: string;
  value: string;
  onValueChange: (value: string) => void;
  items: readonly { value: string; label: string }[];
}) {
  return (
    <BaseSelect.Root items={items} value={value} onValueChange={(next) => next !== null && onValueChange(next)}>
      <BaseSelect.Trigger {...(id !== undefined && { id })} {...stylex.props(fieldStyles.field, styles.trigger)}>
        <BaseSelect.Value />
        <BaseSelect.Icon {...stylex.props(styles.indicator)}>
          <HugeiconsIcon icon={UnfoldMoreIcon} {...stylex.props(styles.icon)} />
        </BaseSelect.Icon>
      </BaseSelect.Trigger>
      <BaseSelect.Portal>
        <BaseSelect.Positioner sideOffset={4} alignItemWithTrigger={false} {...stylex.props(styles.positioner)}>
          <BaseSelect.Popup {...stylex.props(styles.popup)}>
            {items.map((item) => (
              <BaseSelect.Item
                key={item.value}
                value={item.value}
                className={(state) =>
                  stylex.props(styles.item, state.highlighted && styles.highlighted).className ?? ""
                }
              >
                <BaseSelect.ItemText>{item.label}</BaseSelect.ItemText>
                <BaseSelect.ItemIndicator {...stylex.props(styles.indicator)}>
                  <HugeiconsIcon icon={Tick02Icon} {...stylex.props(styles.icon)} />
                </BaseSelect.ItemIndicator>
              </BaseSelect.Item>
            ))}
          </BaseSelect.Popup>
        </BaseSelect.Positioner>
      </BaseSelect.Portal>
    </BaseSelect.Root>
  );
}
