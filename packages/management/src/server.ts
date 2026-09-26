import {
  ConnectError,
  type ConnectRouter,
  createConnectRouter,
  createContextValues,
  type Interceptor,
} from "@connectrpc/connect";
import {
  createAsyncIterable,
  encodeEnvelope,
  type UniversalHandler,
  type UniversalServerRequest,
  universalServerRequestFromFetch,
  universalServerResponseToFetch,
} from "@connectrpc/connect/protocol";

import { authService } from "./auth/service.ts";
import { tokenAuthenticator } from "./auth/tokens.ts";
import { deploymentService } from "./deployments/service.ts";
import type { Deps } from "./deps.ts";
import { domainService } from "./domains/service.ts";
import { environmentService } from "./environments/service.ts";
import { AuthService } from "./gen/chunk/management/v1/auth_pb.ts";
import { DeploymentService } from "./gen/chunk/management/v1/deployments_pb.ts";
import { DomainService } from "./gen/chunk/management/v1/domains_pb.ts";
import { EnvironmentService } from "./gen/chunk/management/v1/environment_pb.ts";
import { LogService } from "./gen/chunk/management/v1/logs_pb.ts";
import { ProjectService } from "./gen/chunk/management/v1/projects_pb.ts";
import { SecretService } from "./gen/chunk/management/v1/secrets_pb.ts";
import { projectService } from "./projects/service.ts";
import { authenticate, type Authenticator, authInterceptor } from "./rpc/caller.ts";
import { secretService } from "./secrets/service.ts";

export interface HandlerOptions {
  authenticator?: Authenticator;
  /** Registers more services; a service registered again here replaces the default one. */
  extend?: (router: ConnectRouter) => void;
}

/** The largest RPC message a client may send; release archives go to the release store instead. */
export const maxRpcBytes = 4 * 1024 * 1024;

/** The part of Bun's server a handler needs. */
export interface Server {
  timeout(request: Request, seconds: number): void;
}

/** Serves chunk.management.v1 over Connect, gRPC-Web and gRPC, plus the release store's own URLs. */
export function createHandler(
  deps: Deps,
  options: HandlerOptions = {},
): (request: Request, server?: Server) => Promise<Response> {
  const authenticator = options.authenticator ?? tokenAuthenticator(deps.sql);
  const router = createConnectRouter({
    interceptors: [logUnexpectedErrors, authInterceptor],
    readMaxBytes: maxRpcBytes,
  });
  router
    .service(AuthService, authService(deps))
    .service(ProjectService, projectService(deps))
    .service(DeploymentService, deploymentService(deps))
    .service(SecretService, secretService(deps))
    .service(DomainService, domainService(deps))
    .service(EnvironmentService, environmentService(deps))
    .service(LogService, {});
  options.extend?.(router);
  const rpcs = new Map(router.handlers.map((handler) => [handler.requestPath, handler]));

  /** Connect reads a unary request's whole body before interceptors run, so authentication comes first. */
  async function serveRpc(handler: UniversalHandler, request: Request): Promise<Response> {
    const contextValues = await authenticate(authenticator, handler.method, request.headers);
    const universal = universalServerRequestFromFetch(request, {});
    const call = contextValues
      ? { ...universal, contextValues }
      : { ...emptyMessage(universal), contextValues: createContextValues() };
    return universalServerResponseToFetch(await handler(call));
  }

  return async (request, server) => {
    const { pathname } = new URL(request.url);
    const rpc = rpcs.get(pathname);
    if (rpc) {
      // Bun closes connections idle for 10 seconds, which would cut quiet streams between keepalives.
      if (rpc.method.methodKind === "server_streaming") server?.timeout(request, 0);
      return serveRpc(rpc, request);
    }
    if (pathname === "/healthz") return new Response("ok\n");
    return (await deps.releases.fetch?.(request)) ?? new Response("not found\n", { status: 404 });
  };
}

/** Connect answers errors that are not ConnectErrors with a bare INTERNAL, so keep their details in the log. */
const logUnexpectedErrors: Interceptor = (next) => async (request) => {
  try {
    return await next(request);
  } catch (error) {
    if (!(error instanceof ConnectError)) console.error(`${request.url} failed:`, error);
    throw error;
  }
};

/**
 * The request with an empty message in place of the client's body, which is never read. `authInterceptor` then
 * answers it in the client's protocol.
 */
function emptyMessage(request: UniversalServerRequest): UniversalServerRequest {
  const header = new Headers(request.header);
  for (const name of ["content-length", "content-encoding", "connect-content-encoding", "grpc-encoding"])
    header.delete(name);
  const type = header.get("content-type") ?? "";
  const message = new TextEncoder().encode(/[/+]json\b/.test(type) ? "{}" : "");
  const enveloped = /^application\/(?:connect\+|grpc)/.test(type);
  return { ...request, header, body: createAsyncIterable([enveloped ? encodeEnvelope(0, message) : message]) };
}
