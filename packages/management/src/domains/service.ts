import { create } from "@bufbuild/protobuf";
import { Code, ConnectError, type HandlerContext, type ServiceImpl } from "@connectrpc/connect";
import { and, eq, getTableColumns, gt, ne, sql } from "drizzle-orm";

import { notify } from "../changes.ts";
import { newId, randomToken } from "../crypto.ts";
import type { Deps } from "../deps.ts";
import {
  DnsRecordSchema,
  type Domain,
  DomainSchema,
  type DomainService,
  DomainState,
} from "../gen/chunk/management/v1/domains_pb.ts";
import { loadEnvironment, reachableProjects } from "../projects/store.ts";
import { callerOf, checkProjectAccess } from "../rpc/caller.ts";
import { invalid, notFound, page, pageOf, required, seqAfter, timestamp, unique } from "../rpc/validate.ts";
import { domains, environments, projects } from "../schema.ts";

type DomainRow = typeof domains.$inferSelect & { project_id: string; environment_hostname: string };

const labelPattern = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const challengePrefix = "chunk-domain-verification=";

export function domainService({ db, resolveTxt, edge }: Deps): Partial<ServiceImpl<typeof DomainService>> {
  // The SRV target is the environment's own hostname, which the edge routes whatever hostname a client sends.
  const toDomain = (row: DomainRow): Domain =>
    create(DomainSchema, {
      id: row.id,
      environmentId: row.environment_id,
      hostname: row.hostname,
      state: row.state,
      dnsRecords: [
        create(DnsRecordSchema, {
          type: "TXT",
          name: challengeName(row.hostname),
          value: `${challengePrefix}${row.challenge}`,
        }),
        ...(edge && row.environment_hostname
          ? [
              create(DnsRecordSchema, {
                type: "SRV",
                name: `_minecraft._tcp.${row.hostname}`,
                value: `0 0 ${edge.port} ${row.environment_hostname}`,
              }),
            ]
          : []),
      ],
      createTime: timestamp(row.create_time),
    });

  const selectDomains = () =>
    db
      .select({
        ...getTableColumns(domains),
        project_id: environments.project_id,
        owner_id: projects.owner_id,
        environment_hostname: environments.hostname,
      })
      .from(domains)
      .innerJoin(environments, eq(environments.id, domains.environment_id))
      .innerJoin(projects, eq(projects.id, environments.project_id));

  /** The domain, or undefined when it is missing or of an owner the caller doesn't reach. */
  const loadDomain = async (id: string, context: HandlerContext) => {
    const caller = callerOf(context);
    const [row] = await selectDomains().where(
      and(eq(domains.id, required(id, "domain_id")), reachableProjects(caller)),
    );
    if (row) checkProjectAccess(caller, row.project_id, row.owner_id, "domain");
    return row;
  };

  return {
    async addDomain(request, context) {
      const environment = await loadEnvironment(db, callerOf(context), request.environmentId);
      const hostname = normalizeHostname(request.hostname);
      const [taken] = await db
        .select({ one: sql`1` })
        .from(domains)
        .where(
          and(
            eq(domains.hostname, hostname),
            eq(domains.state, DomainState.VERIFIED),
            ne(domains.environment_id, environment.id),
          ),
        );
      if (taken) throw new ConnectError("another environment already verified this hostname", Code.AlreadyExists);
      await db
        .insert(domains)
        .values({
          id: newId("dom"),
          environment_id: environment.id,
          hostname,
          state: DomainState.PENDING_VERIFICATION,
          challenge: randomToken(24),
        })
        .onConflictDoNothing({ target: [domains.environment_id, domains.hostname] });
      const [row] = await selectDomains().where(
        and(eq(domains.environment_id, environment.id), eq(domains.hostname, hostname)),
      );
      if (!row) throw notFound("domain");
      return { domain: toDomain(row) };
    },

    async verifyDomain(request, context) {
      const domain = await loadDomain(request.domainId, context);
      if (!domain) throw notFound("domain");
      if (domain.state === DomainState.VERIFIED) return { domain: toDomain(domain) };
      const expected = `${challengePrefix}${domain.challenge}`;
      if (!(await lookupTxt(resolveTxt, challengeName(domain.hostname))).includes(expected)) {
        return { domain: toDomain(domain) };
      }
      await unique("another environment already verified this hostname", () =>
        db.update(domains).set({ state: DomainState.VERIFIED }).where(eq(domains.id, domain.id)),
      );
      await notify(db, { kind: "environment", environmentId: domain.environment_id });
      return { domain: toDomain({ ...domain, state: DomainState.VERIFIED }) };
    },

    async listDomains(request, context) {
      const environment = await loadEnvironment(db, callerOf(context), request.environmentId);
      const p = page(request);
      const after = seqAfter(p);
      const rows = await selectDomains()
        .where(
          and(eq(domains.environment_id, environment.id), after === undefined ? undefined : gt(domains.seq, after)),
        )
        .orderBy(domains.seq)
        .limit(p.size + 1);
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.seq.toString());
      return { domains: items.map(toDomain), nextPageToken };
    },

    async removeDomain(request, context) {
      const domain = await loadDomain(request.domainId, context);
      if (domain) {
        await db.delete(domains).where(eq(domains.id, domain.id));
        await notify(db, { kind: "environment", environmentId: domain.environment_id });
      }
      return {};
    },
  };
}

function challengeName(hostname: string): string {
  return `_chunk-challenge.${hostname}`;
}

/** Lowercase, without a trailing dot, as the contract stores and edges match hostnames. */
function normalizeHostname(input: string): string {
  const hostname = input.trim().toLowerCase().replace(/\.$/, "");
  const labels = hostname.split(".");
  if (hostname.length > 253 || labels.length < 2 || !labels.every((label) => labelPattern.test(label))) {
    throw invalid("hostname must be a fully qualified domain name");
  }
  return hostname;
}

async function lookupTxt(resolveTxt: Deps["resolveTxt"], name: string): Promise<string[]> {
  try {
    return (await resolveTxt(name)).map((chunks) => chunks.join(""));
  } catch (error) {
    const code = (error as { code?: unknown }).code;
    if (code === "ENOTFOUND" || code === "ENODATA") return [];
    throw new ConnectError(`DNS lookup failed: ${(error as Error).message}`, Code.Unavailable);
  }
}
