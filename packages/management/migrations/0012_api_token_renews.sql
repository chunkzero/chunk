-- A renewing token's expire_time slides forward each time it authenticates.
alter table api_tokens add column renews boolean not null default false;
