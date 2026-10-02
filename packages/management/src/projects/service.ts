import { create } from "@bufbuild/protobuf";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";
import { and, eq, gt, sql } from "drizzle-orm";

import { notify } from "../changes.ts";
import { newId } from "../crypto.ts";
import type { Deps } from "../deps.ts";
import { advanceRevision, deleteEnvironment } from "../environments/store.ts";
import { SleepingPingMode } from "../gen/chunk/management/v1/common_pb.ts";
import {
  CreateEnvironmentResponseSchema,
  CreateProjectResponseSchema,
  EnvironmentState,
  ProjectService,
} from "../gen/chunk/management/v1/projects_pb.ts";
import { type Caller, callerOf, checkProjectAccess, reachesOwner } from "../rpc/caller.ts";
import { idempotent } from "../rpc/idempotency.ts";
import { invalid, page, pageOf, required, seqAfter, slug, unique } from "../rpc/validate.ts";
import { environments, projects } from "../schema.ts";
import { forkHandlers } from "./forks.ts";
import { hostnameOf, loadEnvironment, loadProject, reachableProjects, toEnvironment, toProject } from "./store.ts";

const maxDrainSeconds = 7 * 24 * 60 * 60;

export function projectService(deps: Deps): Partial<ServiceImpl<typeof ProjectService>> {
  const { db, keys, edge } = deps;
  return {
    async createProject(request, context) {
      const caller = callerOf(context);
      if (caller.projectId !== undefined) {
        throw new ConnectError("a project token cannot create projects", Code.PermissionDenied);
      }
      const name = slug(request.name, "name");
      const ownerId = ownerFor(caller, request.ownerId);
      return idempotent({ db, keys, caller, method: ProjectService.method.createProject, request }, async (tx) => {
        const [row] = await unique("a project with this name already exists", () =>
          tx
            .insert(projects)
            .values({ id: newId("prj"), owner_id: ownerId, name })
            .returning(),
        );
        return create(CreateProjectResponseSchema, { project: row && toProject(row) });
      });
    },

    async getProject(request, context) {
      return { project: toProject(await loadProject(db, callerOf(context), request.projectId)) };
    },

    async listProjects(request, context) {
      const caller = callerOf(context);
      const p = page(request);
      const after = seqAfter(p);
      const rows = await db
        .select()
        .from(projects)
        .where(
          and(
            caller.projectId === undefined ? undefined : eq(projects.id, caller.projectId),
            reachableProjects(caller),
            request.ownerId ? eq(projects.owner_id, request.ownerId) : undefined,
            after === undefined ? undefined : gt(projects.seq, after),
          ),
        )
        .orderBy(projects.seq)
        .limit(p.size + 1);
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.seq.toString());
      return { projects: items.map(toProject), nextPageToken };
    },

    async createEnvironment(request, context) {
      const caller = callerOf(context);
      const name = slug(request.name, "name");
      return idempotent({ db, keys, caller, method: ProjectService.method.createEnvironment, request }, async (tx) => {
        const project = await loadProject(tx, caller, request.projectId);
        const id = newId("env");
        const [row] = await unique("an environment with this name already exists in the project", () =>
          tx
            .insert(environments)
            .values({
              id,
              project_id: project.id,
              name,
              state: EnvironmentState.PENDING,
              sleeping_ping: SleepingPingMode.CACHE,
              hostname: hostnameOf(id, edge),
            })
            .returning(),
        );
        await notify(tx, { kind: "environment", environmentId: id });
        return create(CreateEnvironmentResponseSchema, { environment: row && toEnvironment(row, edge) });
      });
    },

    async getEnvironment(request, context) {
      return { environment: toEnvironment(await loadEnvironment(db, callerOf(context), request.environmentId), edge) };
    },

    async listEnvironments(request, context) {
      const project = await loadProject(db, callerOf(context), request.projectId);
      const p = page(request);
      const after = seqAfter(p);
      const rows = await db
        .select()
        .from(environments)
        .where(
          and(eq(environments.project_id, project.id), after === undefined ? undefined : gt(environments.seq, after)),
        )
        .orderBy(environments.seq)
        .limit(p.size + 1);
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.seq.toString());
      return { environments: items.map((row) => toEnvironment(row, edge)), nextPageToken };
    },

    async updateEnvironment(request, context) {
      const environment = await loadEnvironment(db, callerOf(context), request.environmentId);
      const { sleepingPing } = request;
      if (sleepingPing !== undefined && ![SleepingPingMode.CACHE, SleepingPingMode.WAKE].includes(sleepingPing)) {
        throw invalid("sleeping_ping must be CACHE or WAKE");
      }
      const drain = request.drain;
      if (drain) {
        const { maxAgeSeconds, deadlineSeconds } = drain;
        if (maxAgeSeconds < 1 || deadlineSeconds < 1) throw invalid("drain limits must be positive");
        if (maxAgeSeconds > maxDrainSeconds || deadlineSeconds > maxDrainSeconds) {
          throw invalid("drain limits must be at most 7 days");
        }
        if (deadlineSeconds < maxAgeSeconds) throw invalid("drain deadline_seconds must be at least max_age_seconds");
      }
      const [row] = await db
        .update(environments)
        .set({
          sleeping_ping: sql`coalesce(${sleepingPing ?? null}::smallint, sleeping_ping)`,
          drain_max_age_seconds: sql`coalesce(${drain?.maxAgeSeconds ?? null}::integer, drain_max_age_seconds)`,
          drain_deadline_seconds: sql`coalesce(${drain?.deadlineSeconds ?? null}::integer, drain_deadline_seconds)`,
        })
        .where(eq(environments.id, environment.id))
        .returning();
      if (!row) throw invalid("environment was deleted");
      if (drain) await advanceRevision(db, row.id);
      else await notify(db, { kind: "environment", environmentId: row.id });
      return { environment: toEnvironment(row, edge) };
    },

    async deleteEnvironment(request, context) {
      const caller = callerOf(context);
      // An environment of an owner the caller doesn't reach is left alone like a missing one.
      const [row] = await db
        .select({ project_id: environments.project_id, owner_id: projects.owner_id })
        .from(environments)
        .innerJoin(projects, eq(projects.id, environments.project_id))
        .where(and(eq(environments.id, required(request.environmentId, "environment_id")), reachableProjects(caller)));
      if (row) {
        checkProjectAccess(caller, row.project_id, row.owner_id, "environment");
        await deleteEnvironment(db, request.environmentId);
      }
      return {};
    },

    ...forkHandlers(deps),
  };
}

/**
 * The owner a new project of the caller's gets: the one requested, which must be one of the caller's owners when it is
 * limited to some, or else its only one.
 */
function ownerFor(caller: Caller, requested: string): string {
  const { owners } = caller;
  if (!owners) return requested;
  if (requested) {
    if (!reachesOwner(caller, requested)) {
      throw new ConnectError("the caller cannot create projects for this owner", Code.PermissionDenied);
    }
    return requested;
  }
  const [only, ...others] = owners;
  if (!only) throw new ConnectError("the caller has no owner to create projects for", Code.PermissionDenied);
  if (others.length > 0) throw invalid("owner_id is required when the caller has several owners");
  return only.id;
}
