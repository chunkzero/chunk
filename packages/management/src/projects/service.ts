import { create } from "@bufbuild/protobuf";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";

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
import { forkHandlers } from "./forks.ts";
import {
  type EnvironmentRow,
  hostnameOf,
  loadEnvironment,
  loadProject,
  type ProjectRow,
  toEnvironment,
  toProject,
} from "./store.ts";

const maxDrainSeconds = 7 * 24 * 60 * 60;

export function projectService(deps: Deps): Partial<ServiceImpl<typeof ProjectService>> {
  const { sql, keys, edge } = deps;
  return {
    async createProject(request, context) {
      const caller = callerOf(context);
      if (caller.projectId !== undefined) {
        throw new ConnectError("a project token cannot create projects", Code.PermissionDenied);
      }
      const name = slug(request.name, "name");
      return idempotent({ sql, keys, caller, method: ProjectService.method.createProject, request }, async (tx) => {
        const [row] = await unique(
          "a project with this name already exists",
          () =>
            tx<ProjectRow[]>`
            insert into projects (id, owner_id, name) values (${newId("prj")}, ${request.ownerId}, ${name})
            returning *`,
        );
        return create(CreateProjectResponseSchema, { project: row && toProject(row) });
      });
    },

    async getProject(request, context) {
      return { project: toProject(await loadProject(sql, callerOf(context), request.projectId)) };
    },

    async listProjects(request, context) {
      const caller = callerOf(context);
      const p = page(request);
      const after = seqAfter(p);
      const rows = await sql<ProjectRow[]>`
        select * from projects
        where true
          ${caller.projectId === undefined ? sql`` : sql`and id = ${caller.projectId}`}
          ${request.ownerId ? sql`and owner_id = ${request.ownerId}` : sql``}
          ${after === undefined ? sql`` : sql`and seq > ${after}`}
        order by seq
        limit ${p.size + 1}`;
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.seq.toString());
      return { projects: items.map(toProject), nextPageToken };
    },

    async createEnvironment(request, context) {
      const caller = callerOf(context);
      const name = slug(request.name, "name");
      return idempotent({ sql, keys, caller, method: ProjectService.method.createEnvironment, request }, async (tx) => {
        const project = await loadProject(tx, caller, request.projectId);
        const id = newId("env");
        const [row] = await unique(
          "an environment with this name already exists in the project",
          () =>
            tx<EnvironmentRow[]>`
            insert into environments (id, project_id, name, state, sleeping_ping, hostname)
            values (${id}, ${project.id}, ${name}, ${EnvironmentState.PENDING}, ${SleepingPingMode.CACHE},
              ${hostnameOf(id, edge)})
            returning *`,
        );
        await notify(tx, { kind: "environment", environmentId: id });
        return create(CreateEnvironmentResponseSchema, { environment: row && toEnvironment(row, edge) });
      });
    },

    async getEnvironment(request, context) {
      return { environment: toEnvironment(await loadEnvironment(sql, callerOf(context), request.environmentId), edge) };
    },

    async listEnvironments(request, context) {
      const project = await loadProject(sql, callerOf(context), request.projectId);
      const p = page(request);
      const after = seqAfter(p);
      const rows = await sql<EnvironmentRow[]>`
        select * from environments
        where project_id = ${project.id} ${after === undefined ? sql`` : sql`and seq > ${after}`}
        order by seq
        limit ${p.size + 1}`;
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.seq.toString());
      return { environments: items.map((row) => toEnvironment(row, edge)), nextPageToken };
    },

    async updateEnvironment(request, context) {
      const environment = await loadEnvironment(sql, callerOf(context), request.environmentId);
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
      const [row] = await sql<EnvironmentRow[]>`
        update environments set
          sleeping_ping = coalesce(${sleepingPing ?? null}::smallint, sleeping_ping),
          drain_max_age_seconds = coalesce(${drain?.maxAgeSeconds ?? null}::integer, drain_max_age_seconds),
          drain_deadline_seconds = coalesce(${drain?.deadlineSeconds ?? null}::integer, drain_deadline_seconds)
        where id = ${environment.id}
        returning *`;
      if (!row) throw invalid("environment was deleted");
      if (drain) await advanceRevision(sql, row.id);
      else await notify(sql, { kind: "environment", environmentId: row.id });
      return { environment: toEnvironment(row, edge) };
    },

    async deleteEnvironment(request, context) {
      const caller = callerOf(context);
      const [row] = await sql<{ project_id: string }[]>`
        select project_id from environments where id = ${required(request.environmentId, "environment_id")}`;
      if (row) {
        checkProjectAccess(caller, row.project_id);
        await deleteEnvironment(sql, request.environmentId);
      }
      return {};
    },

    ...forkHandlers(deps),
  };
}
