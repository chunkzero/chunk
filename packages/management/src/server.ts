import { ConnectError, type ConnectRouter, createConnectRouter, type Interceptor } from "@connectrpc/connect";
import { createFetchHandler } from "@connectrpc/connect/protocol";

import { authService } from "./auth/service.ts";
import { tokenAuthenticator } from "./auth/tokens.ts";
import { deploymentService } from "./deployments/service.ts";
import type { Deps } from "./deps.ts";
import { domainService } from "./domains/service.ts";
import { AuthService } from "./gen/chunk/management/v1/auth_pb.ts";
import { DeploymentService } from "./gen/chunk/management/v1/deployments_pb.ts";
import { DomainService } from "./gen/chunk/management/v1/domains_pb.ts";
import { LogService } from "./gen/chunk/management/v1/logs_pb.ts";
import { ProjectService } from "./gen/chunk/management/v1/projects_pb.ts";
import { SecretService } from "./gen/chunk/management/v1/secrets_pb.ts";
import { projectService } from "./projects/service.ts";
import { type Authenticator, authInterceptor } from "./rpc/caller.ts";
import { secretService } from "./secrets/service.ts";

export interface HandlerOptions {
  authenticator?: Authenticator;
  /** Registers more services; a service registered again here replaces the default one. */
  extend?: (router: ConnectRouter) => void;
}

/** Serves chunk.management.v1 over Connect, gRPC-Web and gRPC, plus the release store's own URLs. */
export function createHandler(deps: Deps, options: HandlerOptions = {}): (request: Request) => Promise<Response> {
  const router = createConnectRouter({
    interceptors: [logUnexpectedErrors, authInterceptor(options.authenticator ?? tokenAuthenticator(deps.sql))],
  });
  router
    .service(AuthService, authService(deps))
    .service(ProjectService, projectService(deps))
    .service(DeploymentService, deploymentService(deps))
    .service(SecretService, secretService(deps))
    .service(DomainService, domainService(deps))
    .service(LogService, {});
  options.extend?.(router);
  const rpcs = new Map(router.handlers.map((handler) => [handler.requestPath, createFetchHandler(handler)]));

  return async (request) => {
    const { pathname } = new URL(request.url);
    const rpc = rpcs.get(pathname);
    if (rpc) return rpc(request);
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
