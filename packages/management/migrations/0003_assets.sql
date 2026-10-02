CREATE TABLE "asset_blobs" (
	"project_id" text NOT NULL,
	"sha256" text NOT NULL,
	"size_bytes" bigint NOT NULL,
	"sha1" text NOT NULL,
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "asset_blobs_project_id_sha256_pk" PRIMARY KEY("project_id","sha256")
);
--> statement-breakpoint
CREATE TABLE "asset_revision_blobs" (
	"project_id" text NOT NULL,
	"revision_id" text NOT NULL,
	"sha256" text NOT NULL,
	"pack" boolean NOT NULL,
	CONSTRAINT "asset_revision_blobs_project_id_revision_id_sha256_pk" PRIMARY KEY("project_id","revision_id","sha256")
);
--> statement-breakpoint
CREATE TABLE "asset_revisions" (
	"seq" bigserial NOT NULL,
	"project_id" text NOT NULL,
	"id" text NOT NULL,
	"state" smallint NOT NULL,
	"manifest" "bytea" NOT NULL,
	"size_bytes" bigint NOT NULL,
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "asset_revisions_project_id_id_pk" PRIMARY KEY("project_id","id"),
	CONSTRAINT "asset_revisions_seq_unique" UNIQUE("seq")
);
--> statement-breakpoint
-- Deployments made before asset revisions existed pin none.
ALTER TABLE "deployments" ADD COLUMN "asset_revision_id" text DEFAULT '' NOT NULL;--> statement-breakpoint
ALTER TABLE "deployments" ALTER COLUMN "asset_revision_id" DROP DEFAULT;--> statement-breakpoint
ALTER TABLE "environments" ADD COLUMN "pack_token" text DEFAULT replace(gen_random_uuid()::text || gen_random_uuid()::text, '-', '') NOT NULL;--> statement-breakpoint
ALTER TABLE "projects" ADD COLUMN "asset_head_id" text DEFAULT '' NOT NULL;--> statement-breakpoint
ALTER TABLE "asset_blobs" ADD CONSTRAINT "asset_blobs_project_id_projects_id_fk" FOREIGN KEY ("project_id") REFERENCES "public"."projects"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "asset_revision_blobs" ADD CONSTRAINT "asset_revision_blobs_project_id_revision_id_asset_revisions_project_id_id_fk" FOREIGN KEY ("project_id","revision_id") REFERENCES "public"."asset_revisions"("project_id","id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "asset_revisions" ADD CONSTRAINT "asset_revisions_project_id_projects_id_fk" FOREIGN KEY ("project_id") REFERENCES "public"."projects"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "asset_revisions_project" ON "asset_revisions" USING btree ("project_id","seq");--> statement-breakpoint
ALTER TABLE "environments" ADD CONSTRAINT "environments_pack_token_unique" UNIQUE("pack_token");