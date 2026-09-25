import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";

import { isUniqueViolation } from "../db.ts";

export function invalid(message: string): ConnectError {
  return new ConnectError(message, Code.InvalidArgument);
}

export function notFound(what: string): ConnectError {
  return new ConnectError(`${what} not found`, Code.NotFound);
}

export function failedPrecondition(message: string): ConnectError {
  return new ConnectError(message, Code.FailedPrecondition);
}

export function required(value: string, field: string): string {
  if (!value) throw invalid(`${field} is required`);
  return value;
}

const slugPattern = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;

/** Project and environment names: lowercase DNS labels. */
export function slug(value: string, field: string): string {
  if (!slugPattern.test(value)) {
    throw invalid(`${field} must be 1-63 lowercase letters, digits or hyphens, starting and ending alphanumeric`);
  }
  return value;
}

export function timestamp(date: Date | null | undefined) {
  return date ? timestampFromDate(date) : undefined;
}

const defaultPageSize = 50;
const maxPageSize = 200;

export interface Page {
  size: number;
  /** The key of the last item on the previous page. */
  after: string | undefined;
}

export function page(request: { pageSize: number; pageToken: string }): Page {
  if (request.pageSize < 0) throw invalid("page_size must not be negative");
  const size = Math.min(request.pageSize || defaultPageSize, maxPageSize);
  if (!request.pageToken) return { size, after: undefined };
  const after = Buffer.from(request.pageToken, "base64url").toString();
  if (!after.startsWith("v1:")) throw invalid("page_token is not valid");
  return { size, after: after.slice(3) };
}

/** Trims a query result fetched with `size + 1` rows to one page and its next_page_token. */
export function pageOf<T>(rows: T[], { size }: Page, key: (row: T) => string): { items: T[]; nextPageToken: string } {
  const items = rows.slice(0, size);
  const last = items.at(-1);
  const nextPageToken = rows.length > size && last ? Buffer.from(`v1:${key(last)}`).toString("base64url") : "";
  return { items, nextPageToken };
}

/** Decodes a cursor produced from a bigint `seq` column. */
export function seqAfter({ after }: Page): bigint | undefined {
  if (after === undefined) return undefined;
  if (!/^\d{1,19}$/.test(after)) throw invalid("page_token is not valid");
  return BigInt(after);
}

/** Runs an insert or update, turning a unique constraint violation into ALREADY_EXISTS. */
export async function unique<T>(message: string, write: () => Promise<T>): Promise<T> {
  try {
    return await write();
  } catch (error) {
    if (isUniqueViolation(error)) throw new ConnectError(message, Code.AlreadyExists);
    throw error;
  }
}
