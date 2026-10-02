import type { DescMethod } from "@bufbuild/protobuf";
import {
  Code,
  ConnectError,
  type ConnectRouter,
  createConnectRouter,
  createContextKey,
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

import { authService, type SignInOption } from "./auth/service.ts";
import { tokenAuthenticator } from "./auth/tokens.ts";
import { dashboardHandler } from "./dashboard.ts";
import { deploymentService } from "./deployments/service.ts";
import type { Deps } from "./deps.ts";
import { domainService } from "./domains/service.ts";
import { edgeService } from "./edge/service.ts";
import { environmentService } from "./environments/service.ts";
import { AuthService } from "./gen/chunk/management/v1/auth_pb.ts";
import { DeploymentService } from "./gen/chunk/management/v1/deployments_pb.ts";
import { DomainService } from "./gen/chunk/management/v1/domains_pb.ts";
import { EdgeService } from "./gen/chunk/management/v1/edge_pb.ts";
import { EnvironmentService } from "./gen/chunk/management/v1/environment_pb.ts";
import { LogService } from "./gen/chunk/management/v1/logs_pb.ts";
import { ProjectService } from "./gen/chunk/management/v1/projects_pb.ts";
import { SecretService } from "./gen/chunk/management/v1/secrets_pb.ts";
import { logService } from "./logs/service.ts";
import { projectService } from "./projects/service.ts";
import { type Authenticator, authorize, methodKey } from "./rpc/caller.ts";
import { refuseNul } from "./rpc/validate.ts";
import { secretService } from "./secrets/service.ts";

export interface HandlerOptions {
  authenticator?: Authenticator;
  /** Serves the dashboard's static build from here; unset serves no dashboard. */
  dashboardDir?: string | undefined;
  /**
   * Registers more services; a service registered again here replaces the default one. Services new here are extension
   * services, which only extension identities naming them may call.
   */
  extend?: (router: ConnectRouter) => void;
  /** Methods of extension services that anyone may call without a bearer token; each checks its request itself. */
  publicMethods?: readonly DescMethod[];
  /** What `AuthService.GetSignInOptions` answers with. */
  signInOptions?: readonly SignInOption[];
  /** Serves plain HTTP paths, such as a sign-in flow's; see `createHandler` for which requests reach it. */
  routes?: (request: Request) => Promise<Response | undefined>;
}

/** The largest RPC message a client may send; release archives go to the release store instead. */
export const maxRpcBytes = 4 * 1024 * 1024;

/** The part of Bun's server a handler needs. */
export interface Server {
  timeout(request: Request, seconds: number): void;
}

/**
 * Serves chunk.management.v1 over Connect, gRPC-Web and gRPC, plus plain HTTP. A request is answered by the first of:
 * the RPC its path names, `/healthz`, the release store's URLs, `options.routes`, then the dashboard. So an install's
 * routes can't shadow an RPC or a release URL, and every path they don't answer falls through to the dashboard.
 */
export function createHandler(
  deps: Deps,
  options: HandlerOptions = {},
): (request: Request, server?: Server) => Promise<Response> {
  const authenticator = options.authenticator ?? tokenAuthenticator(deps.db);
  const router = createConnectRouter({ interceptors: [logUnexpectedErrors, refuseNul], readMaxBytes: maxRpcBytes });
  router
    .service(AuthService, authService(deps, options.signInOptions))
    .service(ProjectService, projectService(deps))
    .service(DeploymentService, deploymentService(deps))
    .service(SecretService, secretService(deps))
    .service(DomainService, domainService(deps))
    .service(EnvironmentService, environmentService(deps))
    .service(EdgeService, edgeService(deps))
    .service(LogService, logService(deps));
  const builtIn = new Set(router.handlers.map((handler) => handler.service.typeName));
  options.extend?.(router);
  const services = new Map(router.handlers.map(({ service }) => [service.typeName, service]));
  const extensionServices = new Set([...services.keys()].filter((typeName) => !builtIn.has(typeName)));
  const publicMethods = new Set(options.publicMethods?.map(methodKey));
  for (const method of options.publicMethods ?? []) {
    if (!extensionServices.has(method.parent.typeName))
      throw new Error(`${methodKey(method)} is not an extension method`);
  }
  // Answers refused calls in the client's protocol. A registration's own options cannot reach it.
  const refusals = createConnectRouter({ interceptors: [refuse] });
  for (const service of services.values()) refusals.service(service, {});
  const refusalsByPath = new Map(refusals.handlers.map((handler) => [handler.requestPath, handler]));
  const dashboard = options.dashboardDir === undefined ? undefined : dashboardHandler(options.dashboardDir);
  const rpcs = new Map(router.handlers.map((handler) => [handler.requestPath, handler]));

  /**
   * Every RPC passes here, and runs only once authorized: Connect reads a unary request's whole body before interceptors
   * run, and a registration's own interceptors replace the router's.
   */
  async function serveRpc(handler: UniversalHandler, request: Request): Promise<Response> {
    const verdict = await authorize(authenticator, handler.method, request.headers, extensionServices, publicMethods);
    const universal = universalServerRequestFromFetch(request, {});
    if (!(verdict instanceof ConnectError)) {
      return universalServerResponseToFetch(await handler({ ...universal, contextValues: verdict }));
    }
    const refusal = refusalsByPath.get(handler.requestPath);
    if (!refusal) throw verdict;
    const contextValues = createContextValues().set(refusalKey, verdict);
    return universalServerResponseToFetch(await refusal({ ...emptyMessage(universal), contextValues }));
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
    return (
      (await deps.releases.fetch?.(request)) ??
      (await options.routes?.(request)) ??
      (await dashboard?.(request)) ??
      new Response("not found\n", { status: 404 })
    );
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

const refusalKey = createContextKey<ConnectError | undefined>(undefined, { description: "refusal" });

/** Throws the error a refused call carries, before any implementation runs. */
const refuse: Interceptor = () => (request) => {
  throw request.contextValues.get(refusalKey) ?? new ConnectError("the call was refused", Code.PermissionDenied);
};

/** The request with an empty message in place of the client's body, which is never read. `refuse` then answers it. */
function emptyMessage(request: UniversalServerRequest): UniversalServerRequest {
  const header = new Headers(request.header);
  for (const name of ["content-length", "content-encoding", "connect-content-encoding", "grpc-encoding"])
    header.delete(name);
  const type = header.get("content-type") ?? "";
  const message = new TextEncoder().encode(/[/+]json\b/.test(type) ? "{}" : "");
  const enveloped = /^application\/(?:connect\+|grpc)/.test(type);
  return { ...request, header, body: createAsyncIterable([enveloped ? encodeEnvelope(0, message) : message]) };
}
