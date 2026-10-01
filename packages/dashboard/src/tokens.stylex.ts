import * as stylex from "@stylexjs/stylex";

const dark = "@media (prefers-color-scheme: dark)";

/* OpenSSL seed b20cee4947897183ae61662c462d4280: jade, amber, warm paper. */
export const colors = stylex.defineVars({
  background: { default: "oklch(0.976 0.008 85)", [dark]: "oklch(0.21 0.003 100)" },
  foreground: { default: "oklch(0.24 0.025 175)", [dark]: "oklch(0.95 0.005 100)" },
  card: { default: "oklch(0.995 0.003 85)", [dark]: "oklch(0.235 0.003 100)" },
  primary: { default: "oklch(0.43 0.085 178)", [dark]: "oklch(0.79 0.045 155)" },
  primaryForeground: { default: "oklch(0.99 0.008 170)", [dark]: "oklch(0.22 0.008 155)" },
  accent: { default: "oklch(0.925 0.028 175)", [dark]: "oklch(0.29 0.008 155)" },
  accentForeground: { default: "oklch(0.32 0.065 178)", [dark]: "oklch(0.95 0.005 100)" },
  mutedForeground: { default: "oklch(0.49 0.025 175)", [dark]: "oklch(0.77 0.005 100)" },
  destructive: { default: "oklch(0.53 0.19 25)", [dark]: "oklch(0.72 0.14 25)" },
  border: { default: "oklch(0.885 0.016 150)", [dark]: "oklch(0.32 0.003 100)" },
  input: { default: "oklch(0.81 0.025 160)", [dark]: "oklch(0.38 0.004 100)" },
  ring: { default: "oklch(0.55 0.105 178)", [dark]: "oklch(0.73 0.045 155)" },
  link: { default: "oklch(0.43 0.085 178)", [dark]: "oklch(0.83 0.045 155)" },
  success: { default: "oklch(0.52 0.12 155)", [dark]: "oklch(0.76 0.085 155)" },
  warning: { default: "oklch(0.61 0.13 73)", [dark]: "oklch(0.8 0.1 80)" },
});

/** Multiples of the 0.275rem spacing unit: `s2_5` is 2.5 units. */
export const space = stylex.defineVars({
  s1: "0.275rem",
  s1_5: "0.4125rem",
  s2: "0.55rem",
  s2_5: "0.6875rem",
  s3: "0.825rem",
  s4: "1.1rem",
  s5: "1.375rem",
  s6: "1.65rem",
  s8: "2.2rem",
});

export const radii = stylex.defineVars({
  xs: "0.125rem",
  sm: "8px",
  md: "12px",
  lg: "18px",
  xl: "24px",
});

export const fonts = stylex.defineVars({
  sans: "Inter, ui-sans-serif, system-ui, sans-serif",
  mono: '"JetBrains Mono", ui-monospace, "SF Mono", Menlo, monospace',
});

export const fontSizes = stylex.defineVars({
  xs: "0.75rem",
  sm: "0.875rem",
  base: "1rem",
  lg: "1.125rem",
  xl: "1.25rem",
  xxl: "1.5rem",
});

/** The line height that goes with each of `fontSizes`. */
export const lineHeights = stylex.defineVars({
  xs: "1rem",
  sm: "1.25rem",
  base: "1.5rem",
  lg: "1.75rem",
  xl: "1.75rem",
  xxl: "2rem",
});
