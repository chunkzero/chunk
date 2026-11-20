CREATE TABLE "asset_deletions" (
	"project_id" text NOT NULL,
	"sha256" text NOT NULL,
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "asset_deletions_project_id_sha256_pk" PRIMARY KEY("project_id","sha256")
);
--> statement-breakpoint
ALTER TABLE "asset_revisions" ADD COLUMN "protected_until" timestamp with time zone DEFAULT now() NOT NULL;--> statement-breakpoint
UPDATE "asset_revisions" SET "protected_until" = now() + interval '1 hour';--> statement-breakpoint
ALTER TABLE "asset_deletions" ADD CONSTRAINT "asset_deletions_project_id_projects_id_fk" FOREIGN KEY ("project_id") REFERENCES "public"."projects"("id") ON DELETE cascade ON UPDATE no action;