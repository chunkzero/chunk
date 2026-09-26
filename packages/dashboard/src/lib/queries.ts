import { useQuery } from "@tanstack/react-query";

import { api } from "./client.ts";

const refreshMs = 10_000;

/** Reads every page of a list call. */
async function allPages<Response extends { nextPageToken: string }, Item>(
  list: (pageToken: string) => Promise<Response>,
  items: (response: Response) => Item[],
) {
  const all: Item[] = [];
  let pageToken = "";
  do {
    const response = await list(pageToken);
    all.push(...items(response));
    pageToken = response.nextPageToken;
  } while (pageToken);
  return all;
}

export const usePrincipal = () =>
  useQuery({ queryKey: ["principal"], queryFn: () => api.auth.getCurrentPrincipal({}), staleTime: Infinity });

export const useProjects = () =>
  useQuery({
    queryKey: ["projects"],
    queryFn: () =>
      allPages(
        (pageToken) => api.projects.listProjects({ pageToken }),
        (response) => response.projects,
      ),
  });

export const useProject = (projectId: string) =>
  useQuery({
    queryKey: ["project", projectId],
    queryFn: async () => (await api.projects.getProject({ projectId })).project,
  });

export const useEnvironments = (projectId: string) =>
  useQuery({
    queryKey: ["environments", projectId],
    queryFn: () =>
      allPages(
        (pageToken) => api.projects.listEnvironments({ projectId, pageToken }),
        (response) => response.environments,
      ),
    refetchInterval: refreshMs,
  });

export const useEnvironment = (environmentId: string) =>
  useQuery({
    queryKey: ["environment", environmentId],
    queryFn: async () => (await api.projects.getEnvironment({ environmentId })).environment,
    refetchInterval: refreshMs,
  });

export const useDeployment = (deploymentId: string) =>
  useQuery({
    queryKey: ["deployment", deploymentId],
    queryFn: async () => (await api.deployments.getDeployment({ deploymentId })).deployment,
    enabled: deploymentId !== "",
  });

export const recentDeployments = 50;

export const useDeployments = (environmentId: string) =>
  useQuery({
    queryKey: ["deployments", environmentId],
    queryFn: async () =>
      (await api.deployments.listDeployments({ environmentId, pageSize: recentDeployments })).deployments,
    refetchInterval: refreshMs,
  });

export const useSecrets = (environmentId: string) =>
  useQuery({
    queryKey: ["secrets", environmentId],
    queryFn: () =>
      allPages(
        (pageToken) => api.secrets.listSecrets({ environmentId, pageToken }),
        (response) => response.secrets,
      ),
  });

export const useDomains = (environmentId: string) =>
  useQuery({
    queryKey: ["domains", environmentId],
    queryFn: () =>
      allPages(
        (pageToken) => api.domains.listDomains({ environmentId, pageToken }),
        (response) => response.domains,
      ),
  });
