const units: [Intl.RelativeTimeFormatUnit, number][] = [
    ["year", 31_536_000],
    ["month", 2_592_000],
    ["day", 86_400],
    ["hour", 3_600],
    ["minute", 60],
];

export function timeAgo(iso: string) {
    const seconds = Math.round((Date.parse(iso) - Date.now()) / 1000);
    const format = new Intl.RelativeTimeFormat("en", { numeric: "auto" });
    for (const [unit, size] of units) {
        if (Math.abs(seconds) >= size) return format.format(Math.round(seconds / size), unit);
    }
    return "just now";
}

/// Strips the scheme and trailing `.git` so a repository URL reads like `github.com/owner/repo`.
export function repoLabel(repository: string) {
    return repository
        .replace(/^[a-z]+:\/\//, "")
        .replace(/^git@([^:]+):/, "$1/")
        .replace(/\.git$/, "");
}

export const sections = [
    { slug: "", title: "Health" },
    { slug: "functions", title: "Functions" },
    { slug: "sessions", title: "Sessions" },
    { slug: "players", title: "Players" },
    { slug: "assets", title: "Assets" },
    { slug: "logs", title: "Logs" },
    { slug: "settings", title: "Settings" },
];
