CREATE TABLE "api_tokens" (
	"seq" bigserial NOT NULL,
	"id" text PRIMARY KEY NOT NULL,
	"principal_id" text NOT NULL,
	"name" text NOT NULL,
	"project_id" text,
	"secret_hash" "bytea" NOT NULL,
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	"expire_time" timestamp with time zone,
	"revoke_time" timestamp with time zone,
	"kind" text DEFAULT 'person' NOT NULL,
	"environment_id" text,
	"renews" boolean DEFAULT false NOT NULL,
	CONSTRAINT "api_tokens_seq_unique" UNIQUE("seq"),
	CONSTRAINT "api_tokens_secret_hash_unique" UNIQUE("secret_hash")
);
--> statement-breakpoint
CREATE TABLE "blocked_addresses" (
	"environment_id" text NOT NULL,
	"address" text NOT NULL,
	"expire_time" timestamp with time zone NOT NULL,
	CONSTRAINT "blocked_addresses_environment_id_address_pk" PRIMARY KEY("environment_id","address")
);
--> statement-breakpoint
CREATE TABLE "capacity_requests" (
	"environment_id" text NOT NULL,
	"request_id" text NOT NULL,
	"workload" smallint NOT NULL,
	"machine_profile" text NOT NULL,
	"release_id" text NOT NULL,
	"app_id" text NOT NULL,
	"memory_mib" integer NOT NULL,
	"java_version" integer,
	"state" smallint NOT NULL,
	"message" text DEFAULT '' NOT NULL,
	"machine_id" text DEFAULT '' NOT NULL,
	"machine_addresses" text[] DEFAULT '{}' NOT NULL,
	"torn_down" boolean DEFAULT false NOT NULL,
	"started" boolean DEFAULT false NOT NULL,
	"resuming" boolean DEFAULT false NOT NULL,
	"credential" "bytea" NOT NULL,
	"credential_digest" "bytea" NOT NULL,
	"owner_instance_id" text NOT NULL,
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "capacity_requests_environment_id_request_id_pk" PRIMARY KEY("environment_id","request_id")
);
--> statement-breakpoint
CREATE TABLE "deployments" (
	"seq" bigserial NOT NULL,
	"id" text PRIMARY KEY NOT NULL,
	"environment_id" text NOT NULL,
	"release_id" text NOT NULL,
	"state" smallint NOT NULL,
	"trigger" smallint NOT NULL,
	"message" text DEFAULT '' NOT NULL,
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	"update_time" timestamp with time zone DEFAULT now() NOT NULL,
	"activate_time" timestamp with time zone,
	"stop_previous" boolean DEFAULT false NOT NULL,
	CONSTRAINT "deployments_seq_unique" UNIQUE("seq")
);
--> statement-breakpoint
CREATE TABLE "domains" (
	"seq" bigserial NOT NULL,
	"id" text PRIMARY KEY NOT NULL,
	"environment_id" text NOT NULL,
	"hostname" text NOT NULL,
	"state" smallint NOT NULL,
	"challenge" text NOT NULL,
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "domains_seq_unique" UNIQUE("seq"),
	CONSTRAINT "domains_environment_id_hostname_unique" UNIQUE("environment_id","hostname")
);
--> statement-breakpoint
CREATE TABLE "environments" (
	"seq" bigserial NOT NULL,
	"id" text PRIMARY KEY NOT NULL,
	"project_id" text NOT NULL,
	"name" text NOT NULL,
	"state" smallint NOT NULL,
	"active_deployment_id" text DEFAULT '' NOT NULL,
	"hostname" text DEFAULT '' NOT NULL,
	"sleeping_ping" smallint NOT NULL,
	"online_players" integer DEFAULT 0 NOT NULL,
	"forked_from_environment_id" text DEFAULT '' NOT NULL,
	"forked_from_snapshot_id" text DEFAULT '' NOT NULL,
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	"revision" bigint DEFAULT 1 NOT NULL,
	"lease" bigint DEFAULT 0 NOT NULL,
	"epoch" bigint DEFAULT 0 NOT NULL,
	"owner_instance_id" text DEFAULT '' NOT NULL,
	"owner_since" timestamp with time zone,
	"gateway_addresses" text[] DEFAULT '{}' NOT NULL,
	"pings" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"report_sequence" bigint DEFAULT 0 NOT NULL,
	"report_desired_revision" bigint DEFAULT 0 NOT NULL,
	"ready_to_suspend" boolean DEFAULT false NOT NULL,
	"report_logins" bigint DEFAULT 0 NOT NULL,
	"alarm_epoch" bigint DEFAULT 0 NOT NULL,
	"alarm_generation" bigint DEFAULT 0 NOT NULL,
	"alarm_due_seconds" bigint,
	"alarm_due_nanos" integer,
	"alarm_fired" boolean DEFAULT false NOT NULL,
	"machine_id" text DEFAULT '' NOT NULL,
	"machine_addresses" text[] DEFAULT '{}' NOT NULL,
	"machine_token" "bytea",
	"wake_window_start" timestamp with time zone DEFAULT now() NOT NULL,
	"wake_count" integer DEFAULT 0 NOT NULL,
	"wake_logins_pending" integer DEFAULT 0 NOT NULL,
	"drain_max_age_seconds" integer DEFAULT 10800 NOT NULL,
	"drain_deadline_seconds" integer DEFAULT 14400 NOT NULL,
	CONSTRAINT "environments_seq_unique" UNIQUE("seq"),
	CONSTRAINT "environments_project_id_name_unique" UNIQUE("project_id","name"),
	CONSTRAINT "environments_drain_max_age_seconds_check" CHECK ("environments"."drain_max_age_seconds" > 0),
	CONSTRAINT "environments_drain_deadline_seconds_check" CHECK ("environments"."drain_deadline_seconds" > 0),
	CONSTRAINT "drain_deadline_after_max_age" CHECK ("environments"."drain_deadline_seconds" >= "environments"."drain_max_age_seconds")
);
--> statement-breakpoint
CREATE TABLE "idempotent_requests" (
	"scope" text NOT NULL,
	"request_id" text NOT NULL,
	"method" text NOT NULL,
	"fingerprint" "bytea" NOT NULL,
	"response" "bytea",
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "idempotent_requests_scope_request_id_pk" PRIMARY KEY("scope","request_id")
);
--> statement-breakpoint
CREATE TABLE "installation" (
	"id" text PRIMARY KEY NOT NULL
);
--> statement-breakpoint
CREATE TABLE "log_entries" (
	"seq" bigserial PRIMARY KEY NOT NULL,
	"environment_id" text NOT NULL,
	"instance_id" text NOT NULL,
	"sequence" bigint NOT NULL,
	"time" timestamp with time zone NOT NULL,
	"source" smallint NOT NULL,
	"severity" smallint NOT NULL,
	"message" text NOT NULL,
	"app_id" text NOT NULL,
	"deployment_id" text NOT NULL,
	CONSTRAINT "log_entries_environment_id_instance_id_sequence_unique" UNIQUE("environment_id","instance_id","sequence")
);
--> statement-breakpoint
CREATE TABLE "logins" (
	"id_hash" "bytea" PRIMARY KEY NOT NULL,
	"user_code" text NOT NULL,
	"client_name" text NOT NULL,
	"expire_time" timestamp with time zone NOT NULL,
	"principal_id" text,
	"token_id" text,
	"token_secret" "bytea",
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "logins_user_code_unique" UNIQUE("user_code")
);
--> statement-breakpoint
CREATE TABLE "metric_samples" (
	"environment_id" text NOT NULL,
	"instance_id" text NOT NULL,
	"name" text NOT NULL,
	"labels" jsonb NOT NULL,
	"time" timestamp with time zone NOT NULL,
	"value" double precision NOT NULL,
	CONSTRAINT "metric_samples_environment_id_instance_id_name_labels_pk" PRIMARY KEY("environment_id","instance_id","name","labels")
);
--> statement-breakpoint
CREATE TABLE "projects" (
	"seq" bigserial NOT NULL,
	"id" text PRIMARY KEY NOT NULL,
	"owner_id" text NOT NULL,
	"name" text NOT NULL,
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "projects_seq_unique" UNIQUE("seq"),
	CONSTRAINT "projects_owner_id_name_unique" UNIQUE("owner_id","name")
);
--> statement-breakpoint
CREATE TABLE "reconciler_leader" (
	"epoch" bigint NOT NULL
);
--> statement-breakpoint
CREATE TABLE "releases" (
	"project_id" text NOT NULL,
	"id" text NOT NULL,
	"state" smallint NOT NULL,
	"archive_sha256" text NOT NULL,
	"archive_size_bytes" bigint NOT NULL,
	"manifest" jsonb,
	"create_time" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "releases_project_id_id_pk" PRIMARY KEY("project_id","id")
);
--> statement-breakpoint
CREATE TABLE "secrets" (
	"environment_id" text NOT NULL,
	"name" text NOT NULL,
	"version" bigint NOT NULL,
	"ciphertext" "bytea",
	"update_time" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "secrets_environment_id_name_pk" PRIMARY KEY("environment_id","name")
);
--> statement-breakpoint
CREATE TABLE "superseded_instances" (
	"environment_id" text NOT NULL,
	"instance_id" text NOT NULL,
	"owned_since" timestamp with time zone,
	"superseded_time" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "superseded_instances_environment_id_instance_id_pk" PRIMARY KEY("environment_id","instance_id")
);
--> statement-breakpoint
CREATE TABLE "usage_records" (
	"environment_id" text NOT NULL,
	"id" text NOT NULL,
	"instance_id" text DEFAULT '' NOT NULL,
	"start_time" timestamp with time zone NOT NULL,
	"end_time" timestamp with time zone NOT NULL,
	"player_seconds" bigint NOT NULL,
	CONSTRAINT "usage_records_environment_id_id_pk" PRIMARY KEY("environment_id","id")
);
--> statement-breakpoint
ALTER TABLE "api_tokens" ADD CONSTRAINT "api_tokens_project_id_projects_id_fk" FOREIGN KEY ("project_id") REFERENCES "public"."projects"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "api_tokens" ADD CONSTRAINT "api_tokens_environment_id_environments_id_fk" FOREIGN KEY ("environment_id") REFERENCES "public"."environments"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "blocked_addresses" ADD CONSTRAINT "blocked_addresses_environment_id_environments_id_fk" FOREIGN KEY ("environment_id") REFERENCES "public"."environments"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "capacity_requests" ADD CONSTRAINT "capacity_requests_environment_id_environments_id_fk" FOREIGN KEY ("environment_id") REFERENCES "public"."environments"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "deployments" ADD CONSTRAINT "deployments_environment_id_environments_id_fk" FOREIGN KEY ("environment_id") REFERENCES "public"."environments"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "domains" ADD CONSTRAINT "domains_environment_id_environments_id_fk" FOREIGN KEY ("environment_id") REFERENCES "public"."environments"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "environments" ADD CONSTRAINT "environments_project_id_projects_id_fk" FOREIGN KEY ("project_id") REFERENCES "public"."projects"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "log_entries" ADD CONSTRAINT "log_entries_environment_id_environments_id_fk" FOREIGN KEY ("environment_id") REFERENCES "public"."environments"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "metric_samples" ADD CONSTRAINT "metric_samples_environment_id_environments_id_fk" FOREIGN KEY ("environment_id") REFERENCES "public"."environments"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "releases" ADD CONSTRAINT "releases_project_id_projects_id_fk" FOREIGN KEY ("project_id") REFERENCES "public"."projects"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "secrets" ADD CONSTRAINT "secrets_environment_id_environments_id_fk" FOREIGN KEY ("environment_id") REFERENCES "public"."environments"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "superseded_instances" ADD CONSTRAINT "superseded_instances_environment_id_environments_id_fk" FOREIGN KEY ("environment_id") REFERENCES "public"."environments"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "usage_records" ADD CONSTRAINT "usage_records_environment_id_environments_id_fk" FOREIGN KEY ("environment_id") REFERENCES "public"."environments"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "api_tokens_principal" ON "api_tokens" USING btree ("principal_id","seq");--> statement-breakpoint
CREATE INDEX "api_tokens_environment" ON "api_tokens" USING btree ("environment_id") WHERE "api_tokens"."environment_id" is not null;--> statement-breakpoint
CREATE INDEX "deployments_environment" ON "deployments" USING btree ("environment_id","seq");--> statement-breakpoint
CREATE UNIQUE INDEX "domains_verified_hostname" ON "domains" USING btree ("hostname") WHERE "domains"."state" = 2;--> statement-breakpoint
CREATE INDEX "log_entries_environment" ON "log_entries" USING btree ("environment_id","seq");