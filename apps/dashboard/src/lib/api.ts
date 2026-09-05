import { z } from "zod";

const statusSchema = z.object({
    version: z.string(),
    functions: z.boolean(),
    reconciliation: z.boolean(),
    asset_uploads: z.boolean(),
});

export async function getStatus(token: string, signal: AbortSignal) {
    const response = await fetch("/api/status", {
        headers: { Authorization: `Bearer ${token}` },
        cache: "no-store",
        signal,
    });
    if (response.status === 401) throw new Error("The management token was not accepted.");
    if (!response.ok) throw new Error(`Backend returned HTTP ${response.status}.`);
    return statusSchema.parse(await response.json());
}
