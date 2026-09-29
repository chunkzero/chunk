-- The core instance that asked for the request: the environment's owner when it was recorded. Superseding that
-- instance releases its requests.
alter table capacity_requests add column owner_instance_id text not null default '';
alter table capacity_requests alter column owner_instance_id drop default;
