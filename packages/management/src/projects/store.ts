import { create } from "@bufbuild/protobuf";

import type { Edge } from "../config.ts";
import type { Db } from "../db.ts";
import type { SleepingPingMode } from "../gen/chunk/management/v1/common_pb.ts";
import {
  type Environment,
  EnvironmentSchema,
  type EnvironmentState,
  type Project,
  ProjectSchema,
} from "../gen/chunk/management/v1/projects_pb.ts";
import { type Caller, checkProjectAccess } from "../rpc/caller.ts";
import { notFound, required, timestamp } from "../rpc/validate.ts";

export interface ProjectRow {
  seq: bigint;
  id: string;
  owner_id: string;
  name: string;
  create_time: Date;
}

export interface EnvironmentRow {
  seq: bigint;
  id: string;
  project_id: string;
  name: string;
  state: EnvironmentState;
  active_deployment_id: string;
  hostname: string;
  sleeping_ping: SleepingPingMode;
  drain_max_age_seconds: number;
  drain_deadline_seconds: number;
  online_players: number;
  forked_from_environment_id: string;
  forked_from_snapshot_id: string;
  create_time: Date;
}

export function toProject(row: ProjectRow): Project {
  return create(ProjectSchema, {
    id: row.id,
    ownerId: row.owner_id,
    name: row.name,
    createTime: timestamp(row.create_time),
  });
}

export function toEnvironment(row: EnvironmentRow, edge: Edge | undefined): Environment {
  return create(EnvironmentSchema, {
    id: row.id,
    projectId: row.project_id,
    name: row.name,
    state: row.state,
    activeDeploymentId: row.active_deployment_id,
    hostname: row.hostname,
    sleepingPing: row.sleeping_ping,
    drain: { maxAgeSeconds: row.drain_max_age_seconds, deadlineSeconds: row.drain_deadline_seconds },
    onlinePlayers: row.online_players,
    forkedFromEnvironmentId: row.forked_from_environment_id,
    forkedFromSnapshotId: row.forked_from_snapshot_id,
    createTime: timestamp(row.create_time),
    joinAddress: joinAddress(row.hostname, edge),
  });
}

function joinAddress(hostname: string, edge: Edge | undefined): string {
  if (!hostname) return "";
  return edge && edge.port !== 25_565 ? `${hostname}:${edge.port}` : hostname;
}

export async function findProject(db: Db, id: string): Promise<ProjectRow | undefined> {
  const [row] = await db<ProjectRow[]>`select * from projects where id = ${id}`;
  return row;
}

/** Loads a project the caller may reach, or fails with NOT_FOUND or PERMISSION_DENIED. */
export async function loadProject(db: Db, caller: Caller, id: string): Promise<ProjectRow> {
  checkProjectAccess(caller, required(id, "project_id"));
  const row = await findProject(db, id);
  if (!row) throw notFound("project");
  return row;
}

/**
 * Loads an environment the caller may reach, or fails with NOT_FOUND or PERMISSION_DENIED. `lock` holds the row
 * until the transaction ends, which serializes changes to one environment's deployments.
 */
export async function loadEnvironment(
  db: Db,
  caller: Caller,
  id: string,
  { lock = false, field = "environment_id" } = {},
): Promise<EnvironmentRow> {
  const [row] = await db<EnvironmentRow[]>`
    select * from environments where id = ${required(id, field)} ${lock ? db`for update` : db``}`;
  if (!row) throw notFound("environment");
  checkProjectAccess(caller, row.project_id);
  return row;
}
