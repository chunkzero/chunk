import { and, eq, inArray, isNull, sql } from "drizzle-orm";

import { notify } from "../changes.ts";
import type { Database, Db } from "../db.ts";
import { CapacityState } from "../gen/chunk/management/v1/environment_pb.ts";
import { EnvironmentState } from "../gen/chunk/management/v1/projects_pb.ts";
import { failedPrecondition, notFound } from "../rpc/validate.ts";
import { apiTokens, capacityRequests, environments, supersededInstances } from "../schema.ts";
import { endUsage } from "./reports.ts";

/** The capacity requests a release, or the takeover that supersedes their core instance, releases. */
export const releasable = [CapacityState.PROVISIONING, CapacityState.READY, CapacityState.FAILED];
/** A released request's state: RELEASED once its machine is torn down, RELEASING until the reconciler removes it. */
export const releasedState = sql<CapacityState>`case when ${capacityRequests.torn_down} then ${CapacityState.RELEASED}::smallint else ${CapacityState.RELEASING}::smallint end`;

/** Advances the environment's desired-state revision, so attached processes receive a new message. */
export async function advanceRevision(db: Db, environmentId: string): Promise<void> {
  await db
    .update(environments)
    .set({ revision: sql`${environments.revision} + 1` })
    .where(eq(environments.id, environmentId));
  await notify(db, { kind: "environment", environmentId });
}

/**
 * Makes a core instance the environment's owner under a new, higher lease. Refuses instances another core's attach
 * superseded and epochs lower than the environment's. The previous owner, if another instance, is superseded for
 * good: its capacity requests are released as `ReleaseCapacity` would, its stored usage ends at the takeover, and its
 * gateways and reported login count are dropped. A new lease starts with no accepted status report.
 *
 * The takeover is read once the environment's row is locked, and never precedes the current owner's, so ownership
 * windows never invert or overlap however the claims' transactions interleave. `firstClaim` runs in the claim's
 * transaction when no core attached with an epoch before, as a fork's core does once it has restored.
 */
export async function claimLease(
  db: Database,
  environmentId: string,
  instanceId: string,
  epoch: bigint,
  firstClaim?: (tx: Db) => Promise<void>,
) {
  return db.transaction(async (tx) => {
    const thisEnvironment = eq(environments.id, environmentId);
    const [environment] = await tx
      .select({ epoch: environments.epoch, owner_instance_id: environments.owner_instance_id })
      .from(environments)
      .where(thisEnvironment)
      .for("update");
    if (!environment) throw notFound("environment");
    // As text, which keeps the microseconds a Date would drop.
    const [{ takeover } = { takeover: "" }] = await tx
      .select({ takeover: sql<string>`greatest(clock_timestamp(), ${environments.owner_since})::text` })
      .from(environments)
      .where(thisEnvironment);
    const [superseded] = await tx
      .select({ one: sql`1` })
      .from(supersededInstances)
      .where(
        and(eq(supersededInstances.environment_id, environmentId), eq(supersededInstances.instance_id, instanceId)),
      );
    if (superseded) throw failedPrecondition("another core's attach superseded this instance");
    if (epoch < environment.epoch) {
      throw failedPrecondition(`epoch ${epoch} is lower than the environment's epoch ${environment.epoch}`);
    }
    if (environment.epoch === 0n && epoch > 0n) await firstClaim?.(tx);
    if (environment.owner_instance_id && environment.owner_instance_id !== instanceId) {
      await tx.execute(sql`
        insert into superseded_instances (environment_id, instance_id, owned_since, superseded_time)
        select ${environmentId}, ${environment.owner_instance_id}, owner_since, ${takeover}::timestamptz
        from environments where id = ${environmentId}
        on conflict do nothing`);
      await endUsage(tx, environmentId, environment.owner_instance_id, takeover);
      await tx
        .update(capacityRequests)
        .set({ state: releasedState })
        .where(
          and(
            eq(capacityRequests.environment_id, environmentId),
            eq(capacityRequests.owner_instance_id, environment.owner_instance_id),
            inArray(capacityRequests.state, releasable),
          ),
        );
    }
    const sameOwner = sql`owner_instance_id = ${instanceId}`;
    const [claimed] = await tx
      .update(environments)
      .set({
        lease: sql`lease + 1`,
        epoch,
        owner_instance_id: instanceId,
        owner_since: sql`case when ${sameOwner} then owner_since else ${takeover}::timestamptz end`,
        report_sequence: 0n,
        report_desired_revision: 0n,
        ready_to_suspend: false,
        report_logins: sql`case when ${sameOwner} then report_logins else 0 end`,
        gateway_addresses: sql`case when ${sameOwner} then gateway_addresses else '{}' end`,
      })
      .where(thisEnvironment)
      .returning({ lease: environments.lease });
    await notify(tx, { kind: "environment", environmentId });
    return claimed?.lease ?? 0n;
  });
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
export async function deleteEnvironment(db: Database, environmentId: string): Promise<void> {
  await db.transaction(async (tx) => {
    const thisEnvironment = eq(environments.id, environmentId);
    const [environment] = await tx
      .select({ provisioned: sql<boolean>`${environments.machine_token} is not null` })
      .from(environments)
      .where(thisEnvironment)
      .for("update");
    if (!environment) return;
    if (!environment.provisioned) {
      await tx.delete(environments).where(thisEnvironment);
    } else {
      await tx.update(environments).set({ state: EnvironmentState.DELETING }).where(thisEnvironment);
      await tx
        .update(apiTokens)
        .set({ revoke_time: sql`now()` })
        .where(and(eq(apiTokens.environment_id, environmentId), isNull(apiTokens.revoke_time)));
    }
    await notify(tx, { kind: "environment", environmentId });
  });
}
