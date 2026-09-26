-- Environment and edge tokens authenticate processes rather than people; their principal_id is empty.
alter table api_tokens
  add column kind text not null default 'person',
  add column environment_id text references environments (id) on delete cascade;

create index api_tokens_environment on api_tokens (environment_id) where environment_id is not null;

alter table environments
  -- The desired-state revision Attach streams; every change to what core should serve advances it.
  add column revision bigint not null default 1,
  -- The current core owner: its lease, the highest log epoch seen and its instance.
  add column lease bigint not null default 0,
  add column epoch bigint not null default 0,
  add column owner_instance_id text not null default '',
  -- The latest accepted status report under the current lease.
  add column gateway_addresses text[] not null default '{}',
  add column pings jsonb not null default '{}',
  add column report_sequence bigint not null default 0,
  add column report_desired_revision bigint not null default 0,
  add column ready_to_suspend boolean not null default false,
  -- The stored wake alarm, keyed by (epoch, generation); a null due time means no alarm.
  add column alarm_epoch bigint not null default 0,
  add column alarm_generation bigint not null default 0,
  add column alarm_due_seconds bigint,
  add column alarm_due_nanos integer,
  add column alarm_fired boolean not null default false;

-- Core instances another core's attach replaced; they never own the environment again.
create table superseded_instances (
  environment_id text not null references environments (id) on delete cascade,
  instance_id text not null,
  primary key (environment_id, instance_id)
);

create table usage_records (
  environment_id text not null references environments (id) on delete cascade,
  id text not null,
  start_time timestamptz not null,
  end_time timestamptz not null,
  player_seconds bigint not null,
  primary key (environment_id, id)
);

create table log_entries (
  seq bigserial primary key,
  environment_id text not null references environments (id) on delete cascade,
  instance_id text not null,
  sequence bigint not null,
  time timestamptz not null,
  source smallint not null,
  severity smallint not null,
  message text not null,
  app_id text not null,
  deployment_id text not null,
  unique (environment_id, instance_id, sequence)
);

create index log_entries_environment on log_entries (environment_id, seq);

-- The latest sample of each series.
create table metric_samples (
  environment_id text not null references environments (id) on delete cascade,
  instance_id text not null,
  name text not null,
  labels jsonb not null,
  time timestamptz not null,
  value double precision not null,
  primary key (environment_id, instance_id, name, labels)
);

-- Client addresses (IPv6 by /64) that recently failed authentication at the environment's gateways.
create table blocked_addresses (
  environment_id text not null references environments (id) on delete cascade,
  address text not null,
  expire_time timestamptz not null,
  primary key (environment_id, address)
);
