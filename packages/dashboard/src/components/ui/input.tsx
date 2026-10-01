import { Input as BaseInput } from "@base-ui/react/input";
import * as stylex from "@stylexjs/stylex";

import { colors, fontSizes, lineHeights, radii, space } from "../../tokens.stylex.ts";

const md = "@media (min-width: 48rem)";
const shadow = "0 1px 2px 0 rgb(0 0 0 / 0.05)";

/** The look of a text field, shared by inputs, selects and text areas. */
export const fieldStyles = stylex.create({
  field: {
    width: "100%",
    minWidth: 0,
    borderRadius: radii.md,
    borderWidth: "1px",
    borderStyle: "solid",
    borderColor: { default: colors.input, ":focus-visible": colors.ring },
    backgroundColor: {
      default: "transparent",
      "@media (prefers-color-scheme: dark)": `color-mix(in oklab, ${colors.input} 30%, transparent)`,
    },
    paddingInline: space.s3,
    paddingBlock: space.s1,
    fontSize: { default: fontSizes.base, [md]: fontSizes.sm },
    lineHeight: { default: lineHeights.base, [md]: lineHeights.sm },
    boxShadow: {
      default: shadow,
      ":focus-visible": `0 0 0 3px color-mix(in oklab, ${colors.ring} 50%, transparent), ${shadow}`,
    },
    transitionProperty: "color, box-shadow",
    transitionDuration: "150ms",
    transitionTimingFunction: "cubic-bezier(0.4, 0, 0.2, 1)",
    outlineStyle: "none",
    cursor: { default: null, ":disabled": "not-allowed" },
    opacity: { default: null, ":disabled": 0.5 },
    "::placeholder": { color: colors.mutedForeground },
  },
  invalid: {
    borderColor: colors.destructive,
    boxShadow: {
      default: shadow,
      ":focus-visible": `0 0 0 3px color-mix(in oklab, ${colors.destructive} 20%, transparent), ${shadow}`,
    },
  },
});

const styles = stylex.create({
  input: { height: "2.475rem", minHeight: "2.75rem", paddingInline: "1rem" },
});

export function Input({
  style,
  ...props
}: Omit<BaseInput.Props, "className" | "style"> & { style?: stylex.StyleXStyles }) {
  const invalid = props["aria-invalid"] === true || props["aria-invalid"] === "true";
  return (
    <BaseInput {...props} {...stylex.props(fieldStyles.field, styles.input, invalid && fieldStyles.invalid, style)} />
  );
}
