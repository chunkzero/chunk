import type { DescMethod } from "@bufbuild/protobuf";
import {
  Code,
  ConnectError,
  type ContextValues,
  createContextKey,
  createContextValues,
  type HandlerContext,
  type Interceptor,
} from "@connectrpc/connect";

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

const isPublic = (method: DescMethod) => publicMethods.has(`${method.parent.typeName}/${method.name}`);

/**
 * Authenticates a call from its headers alone, so the server can turn it away before Connect reads its body. Returns
 * the context values the call runs with, or undefined when a protected method has no valid bearer token.
 */
export async function authenticate(
  authenticator: Authenticator,
  method: DescMethod,
  header: Headers,
): Promise<ContextValues | undefined> {
  const values = createContextValues();
  if (isPublic(method)) return values;
  const bearer = /^Bearer (\S+)$/i.exec(header.get("authorization") ?? "")?.[1];
  const caller = bearer === undefined ? undefined : await authenticator.authenticate(bearer);
  if (!caller) return undefined;
  values.set(callerKey, caller);
  return values;
}

/** Rejects protected calls that `authenticate` did not admit. */
export const authInterceptor: Interceptor = (next) => async (request) => {
  if (!isPublic(request.method) && request.contextValues.get(callerKey) === undefined) {
    throw new ConnectError("a valid bearer token is required", Code.Unauthenticated);
  }
  return next(request);
};

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
