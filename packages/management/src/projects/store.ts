import { create } from "@bufbuild/protobuf";
import { eq, getTableColumns, inArray, type SQL } from "drizzle-orm";

import type { Edge } from "../config.ts";
import type { Db } from "../db.ts";
import {
  type Environment,
  EnvironmentSchema,
  type Project,
  ProjectSchema,
} from "../gen/chunk/management/v1/projects_pb.ts";
import { type Caller, checkProjectAccess } from "../rpc/caller.ts";
import { notFound, required, timestamp } from "../rpc/validate.ts";
import { environments, projects } from "../schema.ts";

export type ProjectRow = typeof projects.$inferSelect;
export type EnvironmentRow = typeof environments.$inferSelect;

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

/** The hostname the install assigns a new environment: none without an edge. */
export function hostnameOf(environmentId: string, edge: Edge | undefined): string {
  return edge ? `${environmentId.replace("_", "-")}.${edge.domain}` : "";
}

function joinAddress(hostname: string, edge: Edge | undefined): string {
  if (!hostname) return "";
  return edge && edge.port !== 25_565 ? `${hostname}:${edge.port}` : hostname;
}

export async function findProject(db: Db, id: string): Promise<ProjectRow | undefined> {
  const [row] = await db.select().from(projects).where(eq(projects.id, id));
  return row;
}

/** Narrows a query over `projects` to those the caller may reach; undefined when it reaches every project. */
export function reachableProjects(caller: Caller): SQL | undefined {
  const ownerIds = caller.owners?.map((owner) => owner.id);
  return ownerIds && inArray(projects.owner_id, ownerIds);
}

/** Loads a project the caller may reach, or fails with NOT_FOUND or PERMISSION_DENIED. */
export async function loadProject(db: Db, caller: Caller, id: string): Promise<ProjectRow> {
  const row = await findProject(db, required(id, "project_id"));
  checkProjectAccess(caller, id, row?.owner_id);
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
  const query = db
    .select({ ...getTableColumns(environments), owner_id: projects.owner_id })
    .from(environments)
    .innerJoin(projects, eq(projects.id, environments.project_id))
    .where(eq(environments.id, required(id, field)));
  const [row] = await (lock ? query.for("update", { of: environments }) : query);
  if (!row) throw notFound("environment");
  const { owner_id: ownerId, ...environment } = row;
  checkProjectAccess(caller, environment.project_id, ownerId, "environment");
  return environment;
}
