import { stat } from "node:fs/promises";
import { join, resolve, sep } from "node:path";

const securityHeaders = {
  // The login page approves device logins, so no other site may frame it.
  "content-security-policy": "frame-ancestors 'none'",
  "x-content-type-options": "nosniff",
};

/**
 * Serves the dashboard's static build from `directory`: its files as they are, hashed `/assets/*` as immutable, and
 * `index.html` for every other GET so client-side routes load. Returns undefined for other methods.
 */
export function dashboardHandler(directory: string): (request: Request) => Promise<Response | undefined> {
  const root = resolve(directory);
  return async (request) => {
    if (request.method !== "GET" && request.method !== "HEAD") return undefined;
    const { pathname } = new URL(request.url);
    const assets = pathname.startsWith("/assets/");
    const path = await fileAt(root, pathname);
    if (path) return serve(request, path, assets ? "public, max-age=31536000, immutable" : "no-cache");
    if (assets) return new Response("not found\n", { status: 404, headers: securityHeaders });
    return serve(request, join(root, "index.html"), "no-cache");
  };
}

/** The regular file `pathname` names inside `root`, or undefined for anything else, including escapes. */
async function fileAt(root: string, pathname: string): Promise<string | undefined> {
  let decoded: string;
  try {
    decoded = decodeURIComponent(pathname);
  } catch {
    return undefined;
  }
  if (decoded.includes("\0")) return undefined;
  const path = resolve(root, `.${decoded}`);
  if (!path.startsWith(`${root}${sep}`)) return undefined;
  const info = await stat(path).catch(() => undefined);
  return info?.isFile() ? path : undefined;
}

function serve(request: Request, path: string, cacheControl: string): Response {
  const file = Bun.file(path);
  const headers = { ...securityHeaders, "cache-control": cacheControl, "content-type": file.type };
  return new Response(request.method === "HEAD" ? null : file, { headers });
}
