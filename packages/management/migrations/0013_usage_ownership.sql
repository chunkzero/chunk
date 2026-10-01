-- Each core instance owns the environment from its takeover until another core's attach supersedes it, on management's
-- clock, and its usage spans are cut to that interval.
alter table environments add column owner_since timestamptz;
alter table superseded_instances
  add column owned_since timestamptz,
  add column superseded_time timestamptz not null default now();
alter table usage_records add column instance_id text not null default '';
