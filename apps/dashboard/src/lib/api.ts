import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import createClient from "openapi-fetch";
import type { components, paths } from "./api.generated";
import { useSession } from "./session";

export type Status = components["schemas"]["Status"];
export type SystemSample = components["schemas"]["Sample"];
export type Project = components["schemas"]["Project"];
export type Deployment = components["schemas"]["Deployment"];
export type LogEntry = components["schemas"]["Entry"];
export type TargetSpec = components["schemas"]["TargetSpec"];
type Batch = components["schemas"]["Batch"];
const api = createClient<paths>({ baseUrl: "/api" });

export class UnauthorizedError extends Error {
    constructor() {
        super("The management token was not accepted.");
    }
}

function options(token: string, signal?: AbortSignal) {
    return { headers: { Authorization: `Bearer ${token}` }, cache: "no-store" as const, signal };
}

async function unwrap<T>(
    result: Promise<{ data?: T; error?: unknown; response: Response }>,
): Promise<T> {
    const { data, error, response } = await result;
    if (response.status === 401) throw new UnauthorizedError();
    if (!response.ok) {
        const message =
            error &&
            typeof error === "object" &&
            "message" in error &&
            typeof error.message === "string"
                ? error.message
                : `Request failed (HTTP ${response.status}).`;
        throw new Error(message);
    }
    if (data === undefined) throw new Error("The server returned an empty response.");
    return data;
}

export const getStatus = (token: string, signal?: AbortSignal) =>
    unwrap(api.GET("/status", options(token, signal)));

function useBackend<T>(
    key: [string, ...unknown[]],
    fetcher: (token: string, signal: AbortSignal) => Promise<T>,
    options: { interval?: number; retry?: boolean; staleTime?: number; enabled?: boolean } = {},
) {
    const { token, disconnect } = useSession();
    return useQuery({
        queryKey: [...key, token],
        queryFn: async ({ signal }) => {
            try {
                return await fetcher(token ?? "", signal);
            } catch (error) {
                if (error instanceof UnauthorizedError) disconnect();
                throw error;
            }
        },
        enabled: token !== null && (options.enabled ?? true),
        retry: (count, error) =>
            !(error instanceof UnauthorizedError) && (options.retry ?? true) && count < 2,
        refetchInterval: options.interval ?? false,
        staleTime: options.staleTime,
    });
}

export const useStatus = () => useBackend(["status"], getStatus, { interval: 5_000 });
export const useSystem = () =>
    useBackend(["system"], (token, signal) => unwrap(api.GET("/system", options(token, signal))), {
        interval: 5_000,
    });
export const useProjects = () =>
    useBackend(
        ["projects"],
        (token, signal) => unwrap(api.GET("/projects", options(token, signal))),
        { interval: 30_000 },
    );

/// Whether the backend accepts application edits, with a reason to show when it does not.
export function useEditable() {
    const status = useStatus();
    const editable = status.data?.project_editing === true;
    let reason: string | undefined;
    if (!editable)
        reason = status.isPending
            ? "Loading server settings…"
            : "Editing is unavailable on this server.";
    return { editable, reason };
}

/// Lists the branches a Git remote advertises. Results stay fresh for a minute.
export function useBranches(repository: string | null | undefined) {
    return useBackend(
        ["branches", repository],
        (token, signal) =>
            unwrap(
                api.GET("/git/branches", {
                    ...options(token, signal),
                    params: { query: { repository: repository ?? "" } },
                }),
            ),
        { enabled: !!repository, retry: false, staleTime: 60_000 },
    );
}

export function useLogs() {
    const client = useQueryClient();
    const { token } = useSession();
    return useBackend<Batch>(
        ["logs"],
        async (credential, signal) => {
            const previous = client.getQueryData<Batch>(["logs", token]);
            const next = await unwrap(
                api.GET("/logs", {
                    ...options(credential, signal),
                    params: {
                        query: previous ? { after: previous.cursor, stream: previous.stream } : {},
                    },
                }),
            );
            return {
                ...next,
                truncated: next.truncated || (!next.reset && (previous?.truncated ?? false)),
                entries: (next.reset
                    ? next.entries
                    : [...(previous?.entries ?? []), ...next.entries]
                ).slice(-1000),
            };
        },
        { interval: 2_000 },
    );
}

export function useProject(projectId: string) {
    const projects = useProjects();
    return { ...projects, project: projects.data?.find((project) => project.id === projectId) };
}

function useProjectMutation<Input>(
    projectId: string,
    send: (token: string, input: Input) => Promise<Project>,
) {
    const { token, disconnect } = useSession();
    const client = useQueryClient();
    return useMutation({
        mutationFn: (input: Input) => {
            if (!token) throw new UnauthorizedError();
            return send(token, input);
        },
        onSuccess: async (project) => {
            await client.cancelQueries({ queryKey: ["projects", token] });
            client.setQueryData<Project[]>(["projects", token], (current) =>
                current?.map((entry) => (entry.id === projectId ? project : entry)),
            );
            await client.invalidateQueries({ queryKey: ["projects", token] });
        },
        onError: (error) => {
            if (error instanceof UnauthorizedError) disconnect();
        },
    });
}

export const useUpdateSource = (projectId: string) =>
    useProjectMutation(projectId, (token, input: components["schemas"]["Source"]) =>
        unwrap(
            api.PUT("/projects/{id}/source", {
                ...options(token),
                params: { path: { id: projectId } },
                body: input,
            }),
        ),
    );
export const useAddTarget = (projectId: string) =>
    useProjectMutation(projectId, (token, input: TargetSpec) =>
        unwrap(
            api.POST("/projects/{id}/targets", {
                ...options(token),
                params: { path: { id: projectId } },
                body: input,
            }),
        ),
    );
export const useUpdateTarget = (projectId: string) =>
    useProjectMutation(projectId, (token, input: { target: string; spec: TargetSpec }) =>
        unwrap(
            api.PUT("/projects/{id}/targets/{target}", {
                ...options(token),
                params: { path: { id: projectId, target: input.target } },
                body: input.spec,
            }),
        ),
    );
export const useRemoveTarget = (projectId: string) =>
    useProjectMutation(projectId, (token, target: string) =>
        unwrap(
            api.DELETE("/projects/{id}/targets/{target}", {
                ...options(token),
                params: { path: { id: projectId, target } },
            }),
        ),
    );
