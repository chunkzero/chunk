import { create } from "@bufbuild/protobuf";
import { Code, ConnectError, type HandlerContext, type ServiceImpl } from "@connectrpc/connect";

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
import { loadEnvironment } from "../projects/store.ts";
import { callerOf, checkProjectAccess } from "../rpc/caller.ts";
import { invalid, notFound, page, pageOf, required, seqAfter, timestamp, unique } from "../rpc/validate.ts";

interface DomainRow {
  seq: bigint;
  id: string;
  environment_id: string;
  hostname: string;
  state: DomainState;
  challenge: string;
  create_time: Date;
  project_id: string;
  environment_hostname: string;
}

const labelPattern = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const challengePrefix = "chunk-domain-verification=";

export function domainService({ sql, resolveTxt, edge }: Deps): Partial<ServiceImpl<typeof DomainService>> {
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

  const selectDomains = () => sql`
    select d.*, e.project_id, e.hostname as environment_hostname
    from domains d join environments e on e.id = d.environment_id`;

  const loadDomain = async (id: string, context: HandlerContext) => {
    const [row] = await sql<DomainRow[]>`${selectDomains()} where d.id = ${required(id, "domain_id")}`;
    if (row) checkProjectAccess(callerOf(context), row.project_id);
    return row;
  };

  return {
    async addDomain(request, context) {
      const environment = await loadEnvironment(sql, callerOf(context), request.environmentId);
      const hostname = normalizeHostname(request.hostname);
      const [taken] = await sql`
        select 1 from domains
        where hostname = ${hostname} and state = ${DomainState.VERIFIED} and environment_id <> ${environment.id}`;
      if (taken) throw new ConnectError("another environment already verified this hostname", Code.AlreadyExists);
      await sql`
        insert into domains (id, environment_id, hostname, state, challenge)
        values (${newId("dom")}, ${environment.id}, ${hostname}, ${DomainState.PENDING_VERIFICATION}, ${randomToken(24)})
        on conflict (environment_id, hostname) do nothing`;
      const [row] = await sql<DomainRow[]>`
        ${selectDomains()} where d.environment_id = ${environment.id} and d.hostname = ${hostname}`;
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
      await unique(
        "another environment already verified this hostname",
        () => sql`update domains set state = ${DomainState.VERIFIED} where id = ${domain.id}`,
      );
      await notify(sql, { kind: "environment", environmentId: domain.environment_id });
      return { domain: toDomain({ ...domain, state: DomainState.VERIFIED }) };
    },

    async listDomains(request, context) {
      const environment = await loadEnvironment(sql, callerOf(context), request.environmentId);
      const p = page(request);
      const after = seqAfter(p);
      const rows = await sql<DomainRow[]>`
        ${selectDomains()}
        where d.environment_id = ${environment.id} ${after === undefined ? sql`` : sql`and d.seq > ${after}`}
        order by d.seq
        limit ${p.size + 1}`;
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.seq.toString());
      return { domains: items.map(toDomain), nextPageToken };
    },

    async removeDomain(request, context) {
      const domain = await loadDomain(request.domainId, context);
      if (domain) {
        await sql`delete from domains where id = ${domain.id}`;
        await notify(sql, { kind: "environment", environmentId: domain.environment_id });
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
