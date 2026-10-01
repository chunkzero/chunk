import { notify } from "../changes.ts";
import type { Db, Sql } from "../db.ts";
import { CapacityState } from "../gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState } from "../gen/chunk/management/v1/projects_pb.ts";
import { failedPrecondition, notFound } from "../rpc/validate.ts";
import { endUsage } from "./reports.ts";

/** Advances the environment's desired-state revision, so attached processes receive a new message. */
export async function advanceRevision(db: Db, environmentId: string): Promise<void> {
  await db`update environments set revision = revision + 1 where id = ${environmentId}`;
  await notify(db, { kind: "environment", environmentId });
}

/**
 * Makes a core instance the environment's owner under a new, higher lease. Refuses instances another core's attach
 * superseded and epochs lower than the environment's. The previous owner, if another instance, is superseded for
 * good: its capacity requests are released as `ReleaseCapacity` would, its stored usage ends at the takeover, and its
 * gateways and reported login count are dropped. A new lease starts with no accepted status report.
 *
 * The takeover is read once the environment's row is locked, and never precedes the current owner's, so ownership
 * windows never invert or overlap however the claims' transactions interleave.
 */
export async function claimLease(sql: Sql, environmentId: string, instanceId: string, epoch: bigint) {
  const { lease } = await sql.begin(async (tx) => {
    const [environment] = await tx<{ epoch: bigint; owner_instance_id: string }[]>`
      select epoch, owner_instance_id from environments where id = ${environmentId} for update`;
    if (!environment) throw notFound("environment");
    // As text, which keeps the microseconds a Date would drop.
    const [{ takeover }] = await tx<[{ takeover: string }]>`
      select greatest(clock_timestamp(), owner_since)::text as takeover from environments where id = ${environmentId}`;
    const [superseded] = await tx`
      select 1 from superseded_instances where environment_id = ${environmentId} and instance_id = ${instanceId}`;
    if (superseded) throw failedPrecondition("another core's attach superseded this instance");
    if (epoch < environment.epoch) {
      throw failedPrecondition(`epoch ${epoch} is lower than the environment's epoch ${environment.epoch}`);
    }
    if (environment.owner_instance_id && environment.owner_instance_id !== instanceId) {
      await tx`
        insert into superseded_instances (environment_id, instance_id, owned_since, superseded_time)
        select ${environmentId}, ${environment.owner_instance_id}, owner_since, ${takeover}::timestamptz
        from environments where id = ${environmentId}
        on conflict do nothing`;
      await endUsage(tx, environmentId, environment.owner_instance_id, takeover);
      await tx`
        update capacity_requests
        set state = case when torn_down then ${CapacityState.RELEASED}::smallint else ${CapacityState.RELEASING}::smallint end
        where environment_id = ${environmentId} and owner_instance_id = ${environment.owner_instance_id}
          and state in (${CapacityState.PROVISIONING}, ${CapacityState.READY}, ${CapacityState.FAILED})`;
    }
    const [claimed] = await tx<{ lease: bigint }[]>`
      update environments
      set lease = lease + 1, epoch = ${epoch}, owner_instance_id = ${instanceId},
        owner_since = case when owner_instance_id = ${instanceId} then owner_since else ${takeover}::timestamptz end,
        report_sequence = 0, report_desired_revision = 0, ready_to_suspend = false,
        report_logins = case when owner_instance_id = ${instanceId} then report_logins else 0 end,
        gateway_addresses = case when owner_instance_id = ${instanceId} then gateway_addresses else '{}' end
      where id = ${environmentId}
      returning lease`;
    await notify(tx, { kind: "environment", environmentId });
    return { lease: claimed?.lease ?? 0n };
  });
  return lease;
}

/** Fails calls made under any lease but the current one. */
export function fenceLease(current: bigint, lease: bigint): void {
  if (lease !== current) {
    throw failedPrecondition(
      lease < current ? `lease ${lease} was superseded by lease ${current}` : `lease ${lease} was never granted`,
    );
  }
}

/**
 * Deletes an environment at once when it never had machines; otherwise marks it DELETING, revokes its tokens and
 * leaves removing the machines, then its log objects, then the row, to the reconciler. Core's token is saved before any
 * machine is created, so an environment without one has none, even ones whose create reply was lost.
 */
export async function deleteEnvironment(sql: Sql, environmentId: string): Promise<void> {
  await sql.begin(async (tx) => {
    const [environment] = await tx<{ provisioned: boolean }[]>`
      select machine_token is not null as provisioned from environments where id = ${environmentId} for update`;
    if (!environment) return;
    if (!environment.provisioned) {
      await tx`delete from environments where id = ${environmentId}`;
    } else {
      await tx`update environments set state = ${EnvironmentState.DELETING} where id = ${environmentId}`;
      await tx`update api_tokens set revoke_time = now() where environment_id = ${environmentId} and revoke_time is null`;
    }
    await notify(tx, { kind: "environment", environmentId });
  });
}
