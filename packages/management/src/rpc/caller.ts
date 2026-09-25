import { Code, ConnectError, createContextKey, type HandlerContext, type Interceptor } from "@connectrpc/connect";

import { AuthService } from "../gen/chunk/management/v1/auth_pb.ts";

export interface Principal {
  id: string;
  displayName: string;
}

/** Who a request acts for, as decided by its bearer token. */
export interface Caller {
  principal: Principal;
  tokenId: string;
  /** Set when the token only reaches this project. */
  projectId: string | undefined;
}

/** Resolves a bearer token to a caller; undefined rejects the request. */
export interface Authenticator {
  authenticate(bearer: string): Promise<Caller | undefined>;
}

const callerKey = createContextKey<Caller | undefined>(undefined, { description: "caller" });

const publicMethods = new Set<string>(
  [AuthService.method.startLogin.name, AuthService.method.pollLogin.name].map(
    (name) => `${AuthService.typeName}/${name}`,
  ),
);

export function authInterceptor(authenticator: Authenticator): Interceptor {
  return (next) => async (request) => {
    if (!publicMethods.has(`${request.service.typeName}/${request.method.name}`)) {
      const bearer = /^Bearer (\S+)$/i.exec(request.header.get("authorization") ?? "")?.[1];
      const caller = bearer === undefined ? undefined : await authenticator.authenticate(bearer);
      if (!caller) throw new ConnectError("a valid bearer token is required", Code.Unauthenticated);
      request.contextValues.set(callerKey, caller);
    }
    return next(request);
  };
}

export function callerOf(context: HandlerContext): Caller {
  const caller = context.values.get(callerKey);
  if (!caller) throw new ConnectError("a valid bearer token is required", Code.Unauthenticated);
  return caller;
}

export function checkProjectAccess(caller: Caller, projectId: string): void {
  if (caller.projectId !== undefined && caller.projectId !== projectId) {
    throw new ConnectError("the token does not reach this project", Code.PermissionDenied);
  }
}
