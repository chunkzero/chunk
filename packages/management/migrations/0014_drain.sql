-- How long a deployment keeps running once another replaces it: after the max age its sessions take no reconnects and
-- its players move to the current deployment, and at the deadline its JVMs stop.
alter table environments
  add column drain_max_age_seconds integer not null default 10800 check (drain_max_age_seconds > 0),
  add column drain_deadline_seconds integer not null default 14400 check (drain_deadline_seconds > 0),
  add constraint drain_deadline_after_max_age check (drain_deadline_seconds >= drain_max_age_seconds);
-- Stop the deployments this one replaces at once instead of draining them.
alter table deployments add column stop_previous boolean not null default false;
