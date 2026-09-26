-- Accepted wakes that advanced the revision within the current one-minute window, for the per-environment limit.
alter table environments
  add column wake_window_start timestamptz not null default now(),
  add column wake_count integer not null default 0;
