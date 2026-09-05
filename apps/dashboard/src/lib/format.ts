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

export function duration(totalSeconds: number) {
    const days = Math.floor(totalSeconds / 86_400);
    const hours = Math.floor((totalSeconds % 86_400) / 3_600);
    const minutes = Math.floor((totalSeconds % 3_600) / 60);
    const seconds = totalSeconds % 60;
    if (days > 0) return `${days}d ${hours}h ${minutes}m`;
    if (hours > 0) return `${hours}h ${minutes}m ${seconds}s`;
    if (minutes > 0) return `${minutes}m ${seconds}s`;
    return `${seconds}s`;
}

export function bytes(value: number) {
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let index = 0;
    let amount = value;
    while (amount >= 1024 && index < units.length - 1) {
        amount /= 1024;
        index += 1;
    }
    return `${amount.toFixed(index >= 2 ? 1 : 0)} ${units[index]}`;
}

export const clock = new Intl.DateTimeFormat("en", { hour12: false, timeStyle: "medium" });

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
