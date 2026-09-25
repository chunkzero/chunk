import { create } from "@bufbuild/protobuf";
import type { ServiceImpl } from "@connectrpc/connect";

import type { Deps } from "../deps.ts";
import {
  DeploymentService,
  DeploymentTrigger,
  DeployResponseSchema,
  PromoteResponseSchema,
  RollbackResponseSchema,
} from "../gen/chunk/management/v1/deployments_pb.ts";
import { loadEnvironment } from "../projects/store.ts";
import { callerOf, checkProjectAccess } from "../rpc/caller.ts";
import { idempotent } from "../rpc/idempotency.ts";
import { failedPrecondition, invalid, notFound, page, pageOf, required, seqAfter } from "../rpc/validate.ts";
import { releaseHandlers } from "./releases.ts";
import { createDeployment, type DeploymentRow, toDeployment } from "./store.ts";

export function deploymentService(deps: Deps): Partial<ServiceImpl<typeof DeploymentService>> {
  const { sql, keys } = deps;
  return {
    ...releaseHandlers(deps),

    async deploy(request, context) {
      const caller = callerOf(context);
      const releaseId = required(request.releaseId, "release_id");
      return idempotent({ sql, keys, caller, method: DeploymentService.method.deploy, request }, async (tx) => {
        const environment = await loadEnvironment(tx, caller, request.environmentId, { lock: true });
        const deployment = await createDeployment(tx, environment, releaseId, DeploymentTrigger.DEPLOY);
        return create(DeployResponseSchema, { deployment });
      });
    },

    async promote(request, context) {
      const caller = callerOf(context);
      if (request.sourceEnvironmentId === request.targetEnvironmentId) {
        throw invalid("source and target environments must differ");
      }
      return idempotent({ sql, keys, caller, method: DeploymentService.method.promote, request }, async (tx) => {
        const source = await loadEnvironment(tx, caller, request.sourceEnvironmentId, {
          field: "source_environment_id",
        });
        const target = await loadEnvironment(tx, caller, request.targetEnvironmentId, {
          lock: true,
          field: "target_environment_id",
        });
        if (source.project_id !== target.project_id) throw invalid("both environments must be in one project");
        const [active] = await tx<{ release_id: string }[]>`
          select release_id from deployments where id = ${source.active_deployment_id}`;
        if (!active) throw failedPrecondition("the source environment has no active deployment");
        const deployment = await createDeployment(tx, target, active.release_id, DeploymentTrigger.PROMOTE);
        return create(PromoteResponseSchema, { deployment });
      });
    },

    async rollback(request, context) {
      const caller = callerOf(context);
      return idempotent({ sql, keys, caller, method: DeploymentService.method.rollback, request }, async (tx) => {
        const environment = await loadEnvironment(tx, caller, request.environmentId, { lock: true });
        const [earlier] = request.deploymentId
          ? await tx<{ release_id: string }[]>`
              select release_id from deployments
              where id = ${request.deploymentId} and environment_id = ${environment.id}`
          : await tx<{ release_id: string }[]>`
              select release_id from deployments
              where environment_id = ${environment.id}
                and activate_time is not null
                and id <> ${environment.active_deployment_id}
              order by activate_time desc
              limit 1`;
        if (!earlier) {
          throw request.deploymentId ? notFound("deployment") : failedPrecondition("no earlier deployment was active");
        }
        const deployment = await createDeployment(tx, environment, earlier.release_id, DeploymentTrigger.ROLLBACK);
        return create(RollbackResponseSchema, { deployment });
      });
    },

    async getDeployment(request, context) {
      const [row] = await sql<(DeploymentRow & { project_id: string })[]>`
        select d.*, e.project_id from deployments d join environments e on e.id = d.environment_id
        where d.id = ${required(request.deploymentId, "deployment_id")}`;
      if (!row) throw notFound("deployment");
      checkProjectAccess(callerOf(context), row.project_id);
      return { deployment: toDeployment(row) };
    },

    async listDeployments(request, context) {
      const environment = await loadEnvironment(sql, callerOf(context), request.environmentId);
      const p = page(request);
      const before = seqAfter(p);
      const rows = await sql<DeploymentRow[]>`
        select * from deployments
        where environment_id = ${environment.id} ${before === undefined ? sql`` : sql`and seq < ${before}`}
        order by seq desc
        limit ${p.size + 1}`;
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.seq.toString());
      return { deployments: items.map(toDeployment), nextPageToken };
    },
  };
}
