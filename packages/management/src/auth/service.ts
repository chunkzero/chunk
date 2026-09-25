import { randomInt } from "node:crypto";

import { create } from "@bufbuild/protobuf";
import { durationFromMs, timestampDate, timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";

import { randomToken, sha256 } from "../crypto.ts";
import type { Deps } from "../deps.ts";
import {
  AuthService,
  CreateTokenResponseSchema,
  LoginState,
  PollLoginResponseSchema,
  PrincipalSchema,
} from "../gen/chunk/management/v1/auth_pb.ts";
import { findProject } from "../projects/store.ts";
import { callerOf, checkProjectAccess } from "../rpc/caller.ts";
import { idempotent } from "../rpc/idempotency.ts";
import { failedPrecondition, invalid, notFound, page, pageOf, required, seqAfter } from "../rpc/validate.ts";
import { findToken, issueToken, type TokenRow, toToken } from "./tokens.ts";

const loginLifetimeMs = 10 * 60 * 1000;
const pollIntervalMs = 5 * 1000;
// Consonants only, so codes never spell words; 20^8 codes.
const codeAlphabet = "BCDFGHJKLMNPQRSTVWXZ";

export function authService({ sql, keys, publicUrl }: Deps): Partial<ServiceImpl<typeof AuthService>> {
  return {
    async startLogin(request) {
      const clientName = request.clientName.slice(0, 200) || "chunk CLI";
      const loginId = randomToken();
      const userCode = newUserCode();
      const expireTime = new Date(Date.now() + loginLifetimeMs);
      await sql`delete from logins where expire_time < now() - interval '1 day'`;
      await sql`
        insert into logins (id_hash, user_code, client_name, expire_time)
        values (${sha256(loginId)}, ${userCode}, ${clientName}, ${expireTime})`;
      return {
        loginId,
        userCode,
        verificationUrl: `${publicUrl}/login?code=${userCode}`,
        expireTime: timestampFromDate(expireTime),
        pollInterval: durationFromMs(pollIntervalMs),
      };
    },

    async pollLogin(request) {
      const idHash = sha256(required(request.loginId, "login_id"));
      const [login] = await sql<{ expire_time: Date; token_id: string | null; token_secret: Uint8Array | null }[]>`
        select expire_time, token_id, token_secret from logins where id_hash = ${idHash}`;
      if (!login) throw notFound("login");
      const expired = login.expire_time <= new Date();
      if (!login.token_id || !login.token_secret) {
        return create(PollLoginResponseSchema, { state: expired ? LoginState.EXPIRED : LoginState.PENDING });
      }
      // An approved login answers with its token until it expires, then is gone.
      if (expired) throw notFound("login");
      const token = await findToken(sql, login.token_id);
      const secret = await keys.cipher.open(login.token_secret, loginContext(idHash));
      return create(PollLoginResponseSchema, {
        state: LoginState.APPROVED,
        secret: new TextDecoder().decode(secret),
        token: token && toToken(token),
      });
    },

    async approveLogin(request, context) {
      const caller = callerOf(context);
      if (caller.projectId !== undefined) {
        throw new ConnectError("a project token cannot approve logins", Code.PermissionDenied);
      }
      const userCode = formatUserCode(request.userCode.toUpperCase().replace(/[^A-Z]/g, ""));
      await sql.begin(async (tx) => {
        const [login] = await tx<
          { id_hash: Uint8Array; client_name: string; expire_time: Date; principal_id: string | null }[]
        >`
          select id_hash, client_name, expire_time, principal_id from logins where user_code = ${userCode} for update`;
        if (!login) throw notFound("login");
        if (login.principal_id !== null) {
          if (login.principal_id === caller.principal.id) return;
          throw failedPrecondition("the login was already approved");
        }
        if (login.expire_time <= new Date()) throw failedPrecondition("the login expired");
        const { row, secret } = await issueToken(tx, {
          principalId: caller.principal.id,
          name: login.client_name,
          projectId: undefined,
          expireTime: undefined,
        });
        const sealed = await keys.cipher.seal(new TextEncoder().encode(secret), loginContext(login.id_hash));
        await tx`
          update logins set principal_id = ${caller.principal.id}, token_id = ${row.id}, token_secret = ${sealed}
          where id_hash = ${login.id_hash}`;
      });
      return {};
    },

    async getCurrentPrincipal(_request, context) {
      const caller = callerOf(context);
      const token = await findToken(sql, caller.tokenId);
      return {
        principal: create(PrincipalSchema, caller.principal),
        token: token && toToken(token),
      };
    },

    async createToken(request, context) {
      const caller = callerOf(context);
      const name = required(request.name, "name").slice(0, 200);
      const projectId = request.projectId || undefined;
      if (caller.projectId !== undefined && projectId !== caller.projectId) {
        throw new ConnectError("a project token can only create tokens for its own project", Code.PermissionDenied);
      }
      const expireTime = request.expireTime && timestampDate(request.expireTime);
      if (expireTime && expireTime <= new Date()) throw invalid("expire_time must be in the future");
      return idempotent(
        { sql, keys, caller, method: AuthService.method.createToken, request, sealed: true },
        async (tx) => {
          if (projectId !== undefined && !(await findProject(tx, projectId))) throw notFound("project");
          const { row, secret } = await issueToken(tx, {
            principalId: caller.principal.id,
            name,
            projectId,
            expireTime,
          });
          return create(CreateTokenResponseSchema, { token: toToken(row), secret });
        },
      );
    },

    async listTokens(request, context) {
      const caller = callerOf(context);
      const p = page(request);
      const after = seqAfter(p);
      const rows = await sql<(TokenRow & { seq: bigint })[]>`
        select seq, id, principal_id, name, project_id, create_time, expire_time from api_tokens
        where principal_id = ${caller.principal.id}
          and revoke_time is null
          and (expire_time is null or expire_time > now())
          ${caller.projectId === undefined ? sql`` : sql`and project_id = ${caller.projectId}`}
          ${after === undefined ? sql`` : sql`and seq > ${after}`}
        order by seq
        limit ${p.size + 1}`;
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.seq.toString());
      return { tokens: items.map(toToken), nextPageToken };
    },

    async revokeToken(request, context) {
      const caller = callerOf(context);
      const token = await findToken(sql, required(request.tokenId, "token_id"));
      if (!token || token.principal_id !== caller.principal.id) throw notFound("token");
      checkProjectAccess(caller, token.project_id ?? "");
      await sql`update api_tokens set revoke_time = now() where id = ${token.id} and revoke_time is null`;
      return {};
    },
  };
}

function newUserCode(): string {
  let code = "";
  for (let i = 0; i < 8; i++) code += codeAlphabet[randomInt(codeAlphabet.length)];
  return formatUserCode(code);
}

function loginContext(idHash: Uint8Array): string {
  return `login/${Buffer.from(idHash).toString("hex")}`;
}

function formatUserCode(code: string): string {
  return `${code.slice(0, 4)}-${code.slice(4)}`;
}
