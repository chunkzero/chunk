-- Enum columns store chunk.management.v1 enum numbers.

-- Request IDs are scoped to the calling principal and its token's project scope.
create table idempotent_requests (
  scope text not null,
  request_id text not null,
  method text not null,
  fingerprint bytea not null,
  response bytea,
  create_time timestamptz not null default now(),
  primary key (scope, request_id)
);

create table projects (
  seq bigserial not null unique,
  id text primary key,
  owner_id text not null,
  name text not null,
  create_time timestamptz not null default now(),
  unique (owner_id, name)
);

create table api_tokens (
  seq bigserial not null unique,
  id text primary key,
  principal_id text not null,
  name text not null,
  project_id text references projects (id) on delete cascade,
  secret_hash bytea not null unique,
  create_time timestamptz not null default now(),
  expire_time timestamptz,
  revoke_time timestamptz
);

create index api_tokens_principal on api_tokens (principal_id, seq);

create table logins (
  id_hash bytea primary key,
  user_code text not null unique,
  client_name text not null,
  expire_time timestamptz not null,
  principal_id text,
  token_id text,
  -- The approved token's secret, sealed with the operator key, served to polls until expire_time.
  token_secret bytea,
  create_time timestamptz not null default now()
);

create table environments (
  seq bigserial not null unique,
  id text primary key,
  project_id text not null references projects (id) on delete cascade,
  name text not null,
  state smallint not null,
  active_deployment_id text not null default '',
  hostname text not null default '',
  sleeping_ping smallint not null,
  online_players integer not null default 0,
  forked_from_environment_id text not null default '',
  forked_from_snapshot_id text not null default '',
  create_time timestamptz not null default now(),
  unique (project_id, name)
);

create table releases (
  project_id text not null references projects (id) on delete cascade,
  id text not null,
  state smallint not null,
  archive_sha256 text not null,
  archive_size_bytes bigint not null,
  manifest jsonb,
  create_time timestamptz not null default now(),
  primary key (project_id, id)
);

create table deployments (
  seq bigserial not null unique,
  id text primary key,
  environment_id text not null references environments (id) on delete cascade,
  release_id text not null,
  state smallint not null,
  trigger smallint not null,
  message text not null default '',
  create_time timestamptz not null default now(),
  update_time timestamptz not null default now(),
  activate_time timestamptz
);

create index deployments_environment on deployments (environment_id, seq);

-- A deleted secret keeps its row without a value, so a later set continues its versions.
create table secrets (
  environment_id text not null references environments (id) on delete cascade,
  name text not null,
  version bigint not null,
  ciphertext bytea,
  update_time timestamptz not null default now(),
  primary key (environment_id, name)
);

create table domains (
  seq bigserial not null unique,
  id text primary key,
  environment_id text not null references environments (id) on delete cascade,
  hostname text not null,
  state smallint not null,
  challenge text not null,
  create_time timestamptz not null default now(),
  unique (environment_id, hostname)
);

-- Several environments may claim a hostname, but only one can verify it.
create unique index domains_verified_hostname on domains (hostname) where state = 2;
