import { useQuery, useQueryClient } from "@tanstack/react-query";
import { z } from "zod";
import { useSession } from "./session";

const statusSchema = z.object({
    version: z.string(),
    functions: z.boolean(),
    reconciliation: z.boolean(),
    asset_uploads: z.boolean(),
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

export type Status = z.infer<typeof statusSchema>;
export type Project = z.infer<typeof projectSchema>;
export type Deployment = z.infer<typeof deploymentSchema>;

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
function useBackend<T>(path: string, schema: z.ZodType<T>) {
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
        refetchInterval: (query) => (query.state.status === "error" ? false : 10_000),
    });
}

export const useStatus = () => useBackend("status", statusSchema);
export const useProjects = () => useBackend("projects", z.array(projectSchema));

export function useProject(projectId: string) {
    const projects = useProjects();
    return { ...projects, project: projects.data?.find((project) => project.id === projectId) };
}
