-- When another core's attach superseded each instance; that instance's usage spans end there.
alter table superseded_instances add column superseded_time timestamptz not null default now();
