import { randomInt } from "node:crypto";

import { create } from "@bufbuild/protobuf";
import { durationFromMs, timestampDate, timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";
import { and, eq, gt, isNull, lt, or, sql } from "drizzle-orm";

import { randomToken, sha256 } from "../crypto.ts";
import type { Deps } from "../deps.ts";
import {
  AuthService,
  CreateTokenResponseSchema,
  LoginState,
  OwnerSchema,
  PollLoginResponseSchema,
  PrincipalSchema,
  SignInOptionSchema,
} from "../gen/chunk/management/v1/auth_pb.ts";
import { loadProject } from "../projects/store.ts";
import { callerOf } from "../rpc/caller.ts";
import { idempotent } from "../rpc/idempotency.ts";
import { failedPrecondition, invalid, notFound, page, pageOf, required, seqAfter } from "../rpc/validate.ts";
import { apiTokens, logins } from "../schema.ts";
import { findToken, issueToken, renewingLifetimeMs, tokenColumns, toToken } from "./tokens.ts";

const loginLifetimeMs = 10 * 60 * 1000;
const pollIntervalMs = 5 * 1000;
// Consonants only, so codes never spell words; 20^8 codes.
const codeAlphabet = "BCDFGHJKLMNPQRSTVWXZ";

/** A way to sign in to the dashboard that an install offers besides API tokens. */
export interface SignInOption {
  label: string;
  url: string;
}

export function authService(
  { db, keys, publicUrl }: Deps,
  signInOptions: readonly SignInOption[] = [],
): Partial<ServiceImpl<typeof AuthService>> {
  return {
    getSignInOptions() {
      return { options: signInOptions.map((option) => create(SignInOptionSchema, option)) };
    },

    async startLogin(request) {
      const clientName = request.clientName.slice(0, 200) || "chunk CLI";
      const loginId = randomToken();
      const userCode = newUserCode();
      const expireTime = new Date(Date.now() + loginLifetimeMs);
      await db.delete(logins).where(lt(logins.expire_time, sql`now() - interval '1 day'`));
      await db
        .insert(logins)
        .values({ id_hash: sha256(loginId), user_code: userCode, client_name: clientName, expire_time: expireTime });
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
      const [login] = await db
        .select({ expire_time: logins.expire_time, token_id: logins.token_id, token_secret: logins.token_secret })
        .from(logins)
        .where(eq(logins.id_hash, idHash));
      if (!login) throw notFound("login");
      const expired = login.expire_time <= new Date();
      if (!login.token_id || !login.token_secret) {
        return create(PollLoginResponseSchema, { state: expired ? LoginState.EXPIRED : LoginState.PENDING });
      }
      // An approved login answers with its token until it expires, then is gone.
      if (expired) throw notFound("login");
      const token = await findToken(db, login.token_id);
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
      await db.transaction(async (tx) => {
        const [login] = await tx
          .select({
            id_hash: logins.id_hash,
            client_name: logins.client_name,
            expire_time: logins.expire_time,
            principal_id: logins.principal_id,
          })
          .from(logins)
          .where(eq(logins.user_code, userCode))
          .for("update");
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
          expireTime: new Date(Date.now() + renewingLifetimeMs),
          renews: true,
        });
        const sealed = await keys.cipher.seal(new TextEncoder().encode(secret), loginContext(login.id_hash));
        await tx
          .update(logins)
          .set({ principal_id: caller.principal.id, token_id: row.id, token_secret: sealed })
          .where(eq(logins.id_hash, login.id_hash));
      });
      return {};
    },

    async getCurrentPrincipal(_request, context) {
      const caller = callerOf(context);
      const token = await findToken(db, caller.tokenId);
      return {
        principal: create(PrincipalSchema, caller.principal),
        token: token && toToken(token),
        owners: caller.owners?.map((owner) => create(OwnerSchema, owner)) ?? [],
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
      return idempotent(
        { db, keys, caller, method: AuthService.method.createToken, request, sealed: true },
        async (tx) => {
          // Checked only when minting, so a retry after expire_time still returns the first result.
          if (expireTime && expireTime <= new Date()) throw invalid("expire_time must be in the future");
          if (projectId !== undefined) await loadProject(tx, caller, projectId);
          const { row, secret } = await issueToken(tx, {
            principalId: caller.principal.id,
            name,
            projectId,
            expireTime,
          });
          return { response: create(CreateTokenResponseSchema, { token: toToken(row), secret }), projectId };
        },
      );
    },

    async listTokens(request, context) {
      const caller = callerOf(context);
      const p = page(request);
      const after = seqAfter(p);
      const rows = await db
        .select({ seq: apiTokens.seq, ...tokenColumns })
        .from(apiTokens)
        .where(
          and(
            eq(apiTokens.principal_id, caller.principal.id),
            isNull(apiTokens.revoke_time),
            or(isNull(apiTokens.expire_time), gt(apiTokens.expire_time, sql`now()`)),
            caller.projectId === undefined ? undefined : eq(apiTokens.project_id, caller.projectId),
            after === undefined ? undefined : gt(apiTokens.seq, after),
          ),
        )
        .orderBy(apiTokens.seq)
        .limit(p.size + 1);
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.seq.toString());
      return { tokens: items.map(toToken), nextPageToken };
    },

    async revokeToken(request, context) {
      const caller = callerOf(context);
      const token = await findToken(db, required(request.tokenId, "token_id"));
      if (!token || token.principal_id !== caller.principal.id) throw notFound("token");
      // Owners aren't checked: people can always revoke their own tokens, whatever projects they can reach now.
      if (caller.projectId !== undefined && token.project_id !== caller.projectId) {
        throw new ConnectError("the token does not reach this project", Code.PermissionDenied);
      }
      await db
        .update(apiTokens)
        .set({ revoke_time: sql`now()` })
        .where(and(eq(apiTokens.id, token.id), isNull(apiTokens.revoke_time)));
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
