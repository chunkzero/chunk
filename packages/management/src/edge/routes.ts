import { create } from "@bufbuild/protobuf";

import type { Db } from "../db.ts";
import type { SleepingPingMode } from "../gen/chunk/management/v1/common_pb.ts";
import { DomainState } from "../gen/chunk/management/v1/domains_pb.ts";
import { type Route, RouteSchema } from "../gen/chunk/management/v1/edge_pb.ts";
import { CapacityState } from "../gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState } from "../gen/chunk/management/v1/projects_pb.ts";

interface RouteRow {
  id: string;
  hostname: string;
  state: EnvironmentState;
  sleeping_ping: SleepingPingMode;
  gateway_addresses: string[];
  pings: Record<string, string>;
  /** Every address of a machine provisioned for the environment. */
  machine_addresses: string[];
  domains: string[];
}

/**
 * Every hostname's route, keyed by hostname: environment hostnames and verified custom domains. Gateways are listed
 * only while the environment runs, and only on machines provisioned for it.
 */
export async function routeTable(db: Db): Promise<Map<string, Route>> {
  const rows = await db<RouteRow[]>`
    select e.id, e.hostname, e.state, e.sleeping_ping, e.gateway_addresses, e.pings,
      e.machine_addresses || coalesce((
        select array_agg(address)
        from capacity_requests c, unnest(c.machine_addresses) address
        where c.environment_id = e.id and c.state = ${CapacityState.READY}
      ), '{}') as machine_addresses,
      coalesce((
        select array_agg(d.hostname) from domains d
        where d.environment_id = e.id and d.state = ${DomainState.VERIFIED}
      ), '{}') as domains
    from environments e
    where e.state <> ${EnvironmentState.DELETING}
    order by e.seq`;
  const routes = new Map<string, Route>();
  for (const row of rows) {
    const provisioned = new Set(row.machine_addresses);
    const gateways =
      row.state === EnvironmentState.RUNNING
        ? row.gateway_addresses.filter((address) => provisioned.has(hostOf(address)))
        : [];
    for (const hostname of [row.hostname, ...row.domains]) {
      if (!hostname || routes.has(hostname)) continue;
      routes.set(
        hostname,
        create(RouteSchema, {
          hostname,
          environmentId: row.id,
          gatewayAddresses: gateways,
          asleep: row.state === EnvironmentState.SUSPENDED,
          sleepingPing: row.sleeping_ping,
          cachedStatusJson: row.pings[hostname] ?? "",
        }),
      );
    }
  }
  return routes;
}

/** The host of `host:port` or `[v6]:port`. */
function hostOf(address: string): string {
  const host = address.slice(0, address.lastIndexOf(":"));
  return host.startsWith("[") ? host.slice(1, -1) : host;
}
