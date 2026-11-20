ALTER TABLE "deployments" ADD COLUMN "drain_until" timestamp with time zone;
--> statement-breakpoint
UPDATE "deployments" SET "drain_until" = "deployments"."update_time" + make_interval(secs => (SELECT "drain_deadline_seconds" FROM "environments" WHERE "environments"."id" = "deployments"."environment_id")) WHERE "state" = 5;
