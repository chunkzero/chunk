-- Earlier outcomes don't record their project, so they can't be rechecked on replay; they're dropped instead.
DELETE FROM "idempotent_requests";--> statement-breakpoint
ALTER TABLE "idempotent_requests" ADD COLUMN "project_id" text;