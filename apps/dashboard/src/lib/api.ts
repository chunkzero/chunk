import { useQuery, useQueryClient } from "@tanstack/react-query";
import { z } from "zod";
import { useSession } from "./session";

const statusSchema = z.object({
    version: z.string(),
    functions: z.boolean(),
    reconciliation: z.boolean(),
    asset_uploads: z.boolean(),
    uptime_seconds: z.number(),
    minecraft_bind: z.string(),
    management_bind: z.string(),
    motd: z.string(),
    max_connections: z.number(),
    connections: z.number(),
    dashboard_dir: z.string(),
    projects_file: z.string().nullable(),
});

const logSchema = z.object({
    seq: z.number(),
    time_ms: z.number(),
    level: z.enum(["TRACE", "DEBUG", "INFO", "WARN", "ERROR"]),
    target: z.string(),
    message: z.string(),
});

const deploymentSchema = z.object({
    id: z.string(),
    name: z.string(),
    environment: z.enum(["production", "development", "preview"]),
    git_ref: z.string().nullable(),
    commit: z.string().nullable(),
    deployed_at: z.string().nullable(),
});

const projectSchema = z.object({
    id: z.string(),
    name: z.string(),
    source: z.object({ repository: z.string(), branch: z.string().nullable() }).nullable(),
    deployments: z.array(deploymentSchema),
});

const systemSchema = z.object({
    hostname: z.string().nullable(),
    os: z.string().nullable(),
    cpus: z.number(),
    cpu_percent: z.number(),
    load_average: z.tuple([z.number(), z.number(), z.number()]),
    memory_total: z.number(),
    memory_used: z.number(),
    memory_available: z.number(),
    swap_total: z.number(),
    swap_used: z.number(),
    process_memory: z.number(),
    process_cpu_percent: z.number(),
});

export type Status = z.infer<typeof statusSchema>;
export type SystemSample = z.infer<typeof systemSchema>;
export type Project = z.infer<typeof projectSchema>;
export type Deployment = z.infer<typeof deploymentSchema>;
export type LogEntry = z.infer<typeof logSchema>;

export class UnauthorizedError extends Error {
    constructor() {
        super("The management token was not accepted.");
    }
}

export async function request<T>(
    path: string,
    schema: z.ZodType<T>,
    token: string,
    signal?: AbortSignal,
) {
    const response = await fetch(`/api/${path}`, {
        headers: { Authorization: `Bearer ${token}` },
        cache: "no-store",
        signal,
    });
    if (response.status === 401) throw new UnauthorizedError();
    if (!response.ok) throw new Error(`Backend returned HTTP ${response.status}.`);
    return schema.parse(await response.json());
}

export const getStatus = (token: string, signal?: AbortSignal) =>
    request("status", statusSchema, token, signal);

/// Polls an endpoint while connected and signs out when the token stops being accepted.
function useBackend<T>(path: string, schema: z.ZodType<T>, interval = 10_000) {
    const { token, disconnect } = useSession();
    const client = useQueryClient();
    return useQuery({
        queryKey: [path, token],
        queryFn: async ({ signal }) => {
            try {
                return await request(path, schema, token ?? "", signal);
            } catch (error) {
                if (error instanceof UnauthorizedError) {
                    client.clear();
                    disconnect();
                }
                throw error;
            }
        },
        enabled: token !== null,
        retry: false,
        refetchInterval: (query) => (query.state.status === "error" ? false : interval),
    });
}

export const useStatus = () => useBackend("status", statusSchema, 5_000);
export const useSystem = () => useBackend("system", systemSchema, 5_000);
export const useLogs = () => useBackend("logs", z.array(logSchema), 2_000);
export const useProjects = () => useBackend("projects", z.array(projectSchema));

export function useProject(projectId: string) {
    const projects = useProjects();
    return { ...projects, project: projects.data?.find((project) => project.id === projectId) };
}
