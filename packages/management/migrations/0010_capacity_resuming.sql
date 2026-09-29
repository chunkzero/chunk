-- Whether the reconciler asked a JVM request's suspended machine to resume and has not seen it running since; one that
-- is still not running then fails its request rather than being resumed again.
alter table capacity_requests add column resuming boolean not null default false;
