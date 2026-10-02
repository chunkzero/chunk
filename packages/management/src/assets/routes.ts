import { type SQL, sql } from "drizzle-orm";

import { type Db, fetchRows } from "../db.ts";
import type { Deps } from "../deps.ts";
import { blobKey } from "../releases/store.ts";
import type { Authenticator } from "../rpc/caller.ts";

/** Where environments fetch asset blobs, with their bearer token: this path followed by a blob's SHA-256. */
export const blobPath = "/blobs/";
/** Where players' clients fetch resource packs: this path, an environment's pack token, `/` and a pack's SHA-256. */
const packPath = "/packs/";
const blobPattern = /^\/blobs\/([0-9a-f]{64})$/;
const packPattern = /^\/packs\/([0-9a-f]{64})\/([0-9a-f]{64})$/;

/**
 * Serves the blobs of asset revisions an environment's deployments pin: any of them to the environment's own token, and
 * packs to anyone holding its pack URL. Returns undefined for other paths.
 */
export function assetRoutes({ db, releases }: Deps, authenticator: Authenticator) {
  return async (request: Request): Promise<Response | undefined> => {
    const { pathname } = new URL(request.url);
    if (!pathname.startsWith(blobPath) && !pathname.startsWith(packPath)) return undefined;
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("method not allowed\n", { status: 405 });
    }
    const blob = blobPattern.exec(pathname);
    if (blob?.[1]) {
      const bearer = /^Bearer (\S+)$/i.exec(request.headers.get("authorization") ?? "")?.[1];
      const identity = bearer === undefined ? undefined : await authenticator.authenticate(bearer);
      if (identity?.kind !== "environment") return new Response("an environment token is required\n", { status: 401 });
      const projectId = await pinnedBy(db, sql`e.id = ${identity.environmentId}`, blob[1], false);
      if (projectId === undefined) return notFound();
      return releases.serve(blobKey(projectId, blob[1]), "machines");
    }
    const pack = packPattern.exec(pathname);
    if (!pack?.[1] || !pack[2]) return notFound();
    const projectId = await pinnedBy(db, sql`e.pack_token = ${pack[1]}`, pack[2], true);
    if (projectId === undefined) return notFound();
    const response = await releases.serve(blobKey(projectId, pack[2]), "clients");
    // Addressed by digest, so the bytes at this URL never change.
    if (response.ok) response.headers.set("cache-control", "public, max-age=31536000, immutable");
    return response;
  };
}

/**
 * The project of the environment `environment` selects, when one of its deployments pins a revision holding the blob
 * (as a pack, when `pack`).
 */
async function pinnedBy(db: Db, environment: SQL, sha256: string, pack: boolean): Promise<string | undefined> {
  const [row] = await fetchRows<{ project_id: string }>(
    db,
    sql`
    select e.project_id from environments e
    where ${environment} and exists (
      select 1 from deployments d
      join asset_revision_blobs b on b.project_id = e.project_id and b.revision_id = d.asset_revision_id
      where d.environment_id = e.id and b.sha256 = ${sha256} and (b.pack or not ${pack})
    )`,
  );
  return row?.project_id;
}

function notFound(): Response {
  return new Response("not found\n", { status: 404 });
}
