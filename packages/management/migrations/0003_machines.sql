alter table environments
  -- The provider machine running core, once created, and the token it was created with, sealed.
  add column machine_id text not null default '',
  add column machine_addresses text[] not null default '{}',
  add column machine_token bytea;

-- EnsureCapacity intents. The reconciler provisions PROVISIONING ones and removes the machines of releasing or
-- failed ones.
create table capacity_requests (
  environment_id text not null references environments (id) on delete cascade,
  request_id text not null,
  workload smallint not null,
  machine_profile text not null,
  release_id text not null,
  app_id text not null,
  memory_mib integer not null,
  state smallint not null,
  message text not null default '',
  machine_id text not null default '',
  machine_addresses text[] not null default '{}',
  torn_down boolean not null default false,
  create_time timestamptz not null default now(),
  primary key (environment_id, request_id)
);
