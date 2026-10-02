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
import { callerOf, checkProjectAccess } from "../rpc/caller.ts";
import { idempotent } from "../rpc/idempotency.ts";
import { invalid, page, pageOf, required, seqAfter, slug, unique } from "../rpc/validate.ts";
import { environments, projects } from "../schema.ts";
import { forkHandlers } from "./forks.ts";
import { hostnameOf, loadEnvironment, loadProject, toEnvironment, toProject } from "./store.ts";

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
      return idempotent({ db, keys, caller, method: ProjectService.method.createProject, request }, async (tx) => {
        const [row] = await unique("a project with this name already exists", () =>
          tx
            .insert(projects)
            .values({ id: newId("prj"), owner_id: request.ownerId, name })
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
      const [row] = await db
        .select({ project_id: environments.project_id })
        .from(environments)
        .where(eq(environments.id, required(request.environmentId, "environment_id")));
      if (row) {
        checkProjectAccess(caller, row.project_id);
        await deleteEnvironment(db, request.environmentId);
      }
      return {};
    },

    ...forkHandlers(deps),
  };
}
