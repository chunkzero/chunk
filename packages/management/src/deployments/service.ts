import { create } from "@bufbuild/protobuf";
import type { ServiceImpl } from "@connectrpc/connect";
import { and, desc, eq, getTableColumns, isNotNull, lt, ne } from "drizzle-orm";

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
import { deployments, environments, projects } from "../schema.ts";
import { releaseHandlers } from "./releases.ts";
import { createDeployment, toDeployment } from "./store.ts";

export function deploymentService(deps: Deps): Partial<ServiceImpl<typeof DeploymentService>> {
  const { db, keys } = deps;
  return {
    ...releaseHandlers(deps),

    async deploy(request, context) {
      const caller = callerOf(context);
      const releaseId = required(request.releaseId, "release_id");
      return idempotent({ db, keys, caller, method: DeploymentService.method.deploy, request }, async (tx) => {
        const environment = await loadEnvironment(tx, caller, request.environmentId, { lock: true });
        const deployment = await createDeployment(
          tx,
          environment,
          releaseId,
          DeploymentTrigger.DEPLOY,
          deps.jvmImage,
          request.stopPrevious,
        );
        return create(DeployResponseSchema, { deployment });
      });
    },

    async promote(request, context) {
      const caller = callerOf(context);
      if (request.sourceEnvironmentId === request.targetEnvironmentId) {
        throw invalid("source and target environments must differ");
      }
      return idempotent({ db, keys, caller, method: DeploymentService.method.promote, request }, async (tx) => {
        const source = await loadEnvironment(tx, caller, request.sourceEnvironmentId, {
          field: "source_environment_id",
        });
        const target = await loadEnvironment(tx, caller, request.targetEnvironmentId, {
          lock: true,
          field: "target_environment_id",
        });
        if (source.project_id !== target.project_id) throw invalid("both environments must be in one project");
        const [active] = await tx
          .select({ release_id: deployments.release_id })
          .from(deployments)
          .where(eq(deployments.id, source.active_deployment_id));
        if (!active) throw failedPrecondition("the source environment has no active deployment");
        const deployment = await createDeployment(
          tx,
          target,
          active.release_id,
          DeploymentTrigger.PROMOTE,
          deps.jvmImage,
        );
        return create(PromoteResponseSchema, { deployment });
      });
    },

    async rollback(request, context) {
      const caller = callerOf(context);
      return idempotent({ db, keys, caller, method: DeploymentService.method.rollback, request }, async (tx) => {
        const environment = await loadEnvironment(tx, caller, request.environmentId, { lock: true });
        const releaseOf = tx.select({ release_id: deployments.release_id }).from(deployments);
        const [earlier] = request.deploymentId
          ? await releaseOf.where(
              and(eq(deployments.id, request.deploymentId), eq(deployments.environment_id, environment.id)),
            )
          : await releaseOf
              .where(
                and(
                  eq(deployments.environment_id, environment.id),
                  isNotNull(deployments.activate_time),
                  ne(deployments.id, environment.active_deployment_id),
                ),
              )
              .orderBy(desc(deployments.activate_time))
              .limit(1);
        if (!earlier) {
          throw request.deploymentId ? notFound("deployment") : failedPrecondition("no earlier deployment was active");
        }
        const deployment = await createDeployment(
          tx,
          environment,
          earlier.release_id,
          DeploymentTrigger.ROLLBACK,
          deps.jvmImage,
        );
        return create(RollbackResponseSchema, { deployment });
      });
    },

    async getDeployment(request, context) {
      const [row] = await db
        .select({ ...getTableColumns(deployments), project_id: environments.project_id, owner_id: projects.owner_id })
        .from(deployments)
        .innerJoin(environments, eq(environments.id, deployments.environment_id))
        .innerJoin(projects, eq(projects.id, environments.project_id))
        .where(eq(deployments.id, required(request.deploymentId, "deployment_id")));
      if (!row) throw notFound("deployment");
      checkProjectAccess(callerOf(context), row.project_id, row.owner_id, "deployment");
      return { deployment: toDeployment(row) };
    },

    async listDeployments(request, context) {
      const environment = await loadEnvironment(db, callerOf(context), request.environmentId);
      const p = page(request);
      const before = seqAfter(p);
      const rows = await db
        .select()
        .from(deployments)
        .where(
          and(
            eq(deployments.environment_id, environment.id),
            before === undefined ? undefined : lt(deployments.seq, before),
          ),
        )
        .orderBy(desc(deployments.seq))
        .limit(p.size + 1);
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.seq.toString());
      return { deployments: items.map(toDeployment), nextPageToken };
    },
  };
}
