import { Code, ConnectError, createClient, type Interceptor } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-web";
import { MutationCache, QueryCache, QueryClient } from "@tanstack/react-query";

import { AuthService } from "../gen/chunk/management/v1/auth_pb.ts";
import { DeploymentService } from "../gen/chunk/management/v1/deployments_pb.ts";
import { DomainService } from "../gen/chunk/management/v1/domains_pb.ts";
import { LogService } from "../gen/chunk/management/v1/logs_pb.ts";
import { ProjectService } from "../gen/chunk/management/v1/projects_pb.ts";
import { SecretService } from "../gen/chunk/management/v1/secrets_pb.ts";
import { getToken, setToken } from "./session.ts";

const bearer: Interceptor = (next) => (request) => {
  const token = getToken();
  if (token && !request.header.has("authorization")) request.header.set("authorization", `Bearer ${token}`);
  return next(request);
};

const transport = createConnectTransport({ baseUrl: window.location.origin, interceptors: [bearer] });

export const api = {
  auth: createClient(AuthService, transport),
  projects: createClient(ProjectService, transport),
  deployments: createClient(DeploymentService, transport),
  secrets: createClient(SecretService, transport),
  domains: createClient(DomainService, transport),
  logs: createClient(LogService, transport),
};

const final = new Set([Code.Unauthenticated, Code.PermissionDenied, Code.NotFound, Code.InvalidArgument]);

export const queryClient = new QueryClient({
  queryCache: new QueryCache({ onError: signOutIfRejected }),
  mutationCache: new MutationCache({ onError: signOutIfRejected }),
  defaultOptions: {
    queries: { retry: (count, error) => count < 2 && !final.has(ConnectError.from(error).code) },
  },
});

/** Refetches the queries under each key prefix, such as `["deployments"]`. */
export async function refresh(...keys: string[]) {
  await Promise.all(keys.map((key) => queryClient.invalidateQueries({ queryKey: [key] })));
}

export function signOut() {
  setToken(null);
  queryClient.clear();
}

function signOutIfRejected(error: unknown) {
  if (getToken() !== null && ConnectError.from(error).code === Code.Unauthenticated) signOut();
}

export function errorMessage(error: unknown) {
  return ConnectError.from(error).rawMessage;
}

/** A request ID for mutations that take one; `crypto.randomUUID` needs a secure context, which plain-HTTP installs lack. */
export function newRequestId() {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6]! & 0x0f) | 0x40;
  bytes[8] = (bytes[8]! & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
