import type { DescMethod } from "@bufbuild/protobuf";
import {
  Code,
  ConnectError,
  type ContextValues,
  createContextKey,
  createContextValues,
  type HandlerContext,
} from "@connectrpc/connect";

import { AuthService } from "../gen/chunk/management/v1/auth_pb.ts";
import { EdgeService } from "../gen/chunk/management/v1/edge_pb.ts";
import { EnvironmentService } from "../gen/chunk/management/v1/environment_pb.ts";
import { notFound } from "./validate.ts";

export interface Principal {
  id: string;
  displayName: string;
}

/** An account or team that owns projects, by the ID projects store as `owner_id`. */
export interface Owner {
  id: string;
  displayName: string;
}

/** Who a request acts for, as decided by its bearer token. */
export interface Caller {
  principal: Principal;
  tokenId: string;
  /** Set when the token only reaches this project. */
  projectId: string | undefined;
  /**
   * The owners whose projects the caller may reach; undefined reaches every project. An install's authenticator sets
   * it on each request, so a change in who belongs to an owner applies to tokens already issued.
   */
  owners?: readonly Owner[] | undefined;
}

/**
 * What a bearer token authenticates: a person or CI job, one environment's processes, an edge, or a subject of an
 * install's own authenticator, which may call only the extension service whose type name is `service`.
 */
export type Identity =
  | { kind: "person"; caller: Caller }
  | { kind: "environment"; environmentId: string; tokenId: string }
  | { kind: "edge"; tokenId: string }
  | { kind: "extension"; service: string; subject: string };

/** Resolves a bearer token to an identity; undefined rejects the request. */
export interface Authenticator {
  authenticate(bearer: string): Promise<Identity | undefined>;
}

const callerKey = createContextKey<Caller | undefined>(undefined, { description: "caller" });
const environmentKey = createContextKey<string | undefined>(undefined, { description: "environment" });
const subjectKey = createContextKey<string | undefined>(undefined, { description: "subject" });

/** Services that only one kind of token may call; every other service is for people. */
const serviceKinds = new Map<string, Identity["kind"]>([
  [EnvironmentService.typeName, "environment"],
  [EdgeService.typeName, "edge"],
]);

/** A method's `<service type name>/<method name>`. */
export const methodKey = (method: DescMethod) => `${method.parent.typeName}/${method.name}`;

const publicMethods = new Set(
  [AuthService.method.startLogin, AuthService.method.pollLogin, AuthService.method.getSignInOptions].map(methodKey),
);

/**
 * Authenticates and authorizes a call from its headers alone, so the server can turn it away before Connect reads its
 * body or runs any interceptor or handler. Returns the context values the call runs with, or the error it is refused
 * with: a protected method needs a valid bearer token of the kind its service admits. The services named in
 * `extensionServices` admit only extension identities naming them; every other service admits none. Methods named in
 * `extensionPublicMethods` (by `methodKey`) need no token, and check their requests themselves.
 */
export async function authorize(
  authenticator: Authenticator,
  method: DescMethod,
  header: Headers,
  extensionServices: ReadonlySet<string>,
  extensionPublicMethods: ReadonlySet<string>,
): Promise<ContextValues | ConnectError> {
  const values = createContextValues();
  const key = methodKey(method);
  if (publicMethods.has(key) || extensionPublicMethods.has(key)) return values;
  const bearer = /^Bearer (\S+)$/i.exec(header.get("authorization") ?? "")?.[1];
  const identity = bearer === undefined ? undefined : await authenticator.authenticate(bearer);
  if (!identity) return new ConnectError("a valid bearer token is required", Code.Unauthenticated);
  const service = method.parent.typeName;
  const admitted = extensionServices.has(service)
    ? identity.kind === "extension" && identity.service === service
    : identity.kind === (serviceKinds.get(service) ?? "person");
  if (!admitted) {
    return new ConnectError(`an ${identity.kind} token cannot call ${method.parent.name}`, Code.PermissionDenied);
  }
  if (identity.kind === "person") values.set(callerKey, identity.caller);
  if (identity.kind === "environment") values.set(environmentKey, identity.environmentId);
  if (identity.kind === "extension") values.set(subjectKey, identity.subject);
  return values;
}

export function callerOf(context: HandlerContext): Caller {
  const caller = context.values.get(callerKey);
  if (!caller) throw new ConnectError("a valid bearer token is required", Code.Unauthenticated);
  return caller;
}

/** The environment whose token made the request. */
export function environmentOf(context: HandlerContext): string {
  const environmentId = context.values.get(environmentKey);
  if (!environmentId) throw new ConnectError("an environment token is required", Code.Unauthenticated);
  return environmentId;
}

/** The subject of the extension identity that called an extension service. */
export function subjectOf(context: HandlerContext): string {
  const subject = context.values.get(subjectKey);
  if (subject === undefined) throw new ConnectError("an extension credential is required", Code.Unauthenticated);
  return subject;
}

/** Whether the caller may reach projects of `ownerId`. */
export function reachesOwner(caller: Caller, ownerId: string): boolean {
  return caller.owners?.some((owner) => owner.id === ownerId) ?? true;
}

/**
 * Fails unless the caller may reach the project `projectId` of `ownerId`, which is undefined when no such project
 * exists. A project of an owner the caller doesn't reach fails with NOT_FOUND for `resource`, as a missing one does, so
 * other owners' projects stay hidden. A token limited to another project fails with PERMISSION_DENIED.
 */
export function checkProjectAccess(
  caller: Caller,
  projectId: string,
  ownerId: string | undefined,
  resource = "project",
): void {
  if (caller.owners && (ownerId === undefined || !reachesOwner(caller, ownerId))) throw notFound(resource);
  if (caller.projectId !== undefined && caller.projectId !== projectId) {
    throw new ConnectError("the token does not reach this project", Code.PermissionDenied);
  }
}
