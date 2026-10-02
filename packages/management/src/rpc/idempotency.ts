import { type DescMessage, type DescMethodUnary, fromBinary, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { and, eq } from "drizzle-orm";

import type { Keys } from "../crypto.ts";
import type { Database, Db } from "../db.ts";
import { idempotentRequests, projects } from "../schema.ts";
import { type Caller, checkProjectAccess } from "./caller.ts";

interface Options<I extends DescMessage, O extends DescMessage> {
  db: Database;
  keys: Keys;
  /** Request IDs are scoped to the caller's principal and token scope. */
  caller: Caller;
  method: DescMethodUnary<I, O>;
  request: MessageShape<I> & { requestId: string };
  /** Encrypt the stored response, for responses that carry secrets. */
  sealed?: boolean;
}

/** What a mutation answers, and the project its answer belongs to, if any. */
export interface Outcome<O extends DescMessage> {
  response: MessageShape<O>;
  projectId: string | undefined;
}

/**
 * Runs a mutation once per request_id. The mutation and the recorded response share one transaction, so a failed
 * attempt leaves nothing behind and a concurrent retry waits for the first attempt, then returns its response. A
 * replay returns it only while the caller still reaches the project the mutation names.
 */
export async function idempotent<I extends DescMessage, O extends DescMessage>(
  { db, keys, caller, method, request, sealed = false }: Options<I, O>,
  run: (tx: Db) => Promise<Outcome<O>>,
): Promise<MessageShape<O>> {
  const { requestId } = request;
  if (!requestId || requestId.length > 128) {
    throw new ConnectError("request_id must be set, and at most 128 characters", Code.InvalidArgument);
  }
  const name = `${method.parent.typeName}/${method.name}`;
  const scope = `${caller.principal.id}/${caller.projectId ?? ""}`;
  const fingerprint = keys.fingerprint(Buffer.concat([Buffer.from(`${name}\n`), toBinary(method.input, request)]));
  const context = `idempotent/${scope}/${requestId}`;
  const thisRequest = and(eq(idempotentRequests.scope, scope), eq(idempotentRequests.request_id, requestId));
  return db.transaction(async (tx) => {
    const claimed = await tx
      .insert(idempotentRequests)
      .values({ scope, request_id: requestId, method: name, fingerprint })
      .onConflictDoNothing()
      .returning({ request_id: idempotentRequests.request_id });
    if (claimed.length === 0) {
      const [first] = await tx
        .select({
          method: idempotentRequests.method,
          fingerprint: idempotentRequests.fingerprint,
          response: idempotentRequests.response,
          project_id: idempotentRequests.project_id,
          owner_id: projects.owner_id,
        })
        .from(idempotentRequests)
        .leftJoin(projects, eq(projects.id, idempotentRequests.project_id))
        .where(thisRequest);
      if (!first?.response || first.method !== name || !Buffer.from(first.fingerprint).equals(fingerprint)) {
        throw new ConnectError("request_id was already used with different arguments", Code.AlreadyExists);
      }
      // The caller may have lost the project since; a replay answers as the first attempt would now.
      if (first.project_id !== null) checkProjectAccess(caller, first.project_id, first.owner_id ?? undefined);
      const bytes = sealed ? await keys.cipher.open(first.response, context) : first.response;
      return fromBinary(method.output, bytes);
    }
    const { response, projectId } = await run(tx);
    const bytes = toBinary(method.output, response);
    await tx
      .update(idempotentRequests)
      .set({ response: sealed ? await keys.cipher.seal(bytes, context) : bytes, project_id: projectId ?? null })
      .where(thisRequest);
    return response;
  });
}
