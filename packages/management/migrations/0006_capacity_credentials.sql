-- The credential core minted for a request's machine, sealed, and its keyed digest, which retries are matched on.
-- Requests from before credentials have neither, so unfinished ones fail (3) and the reconciler removes their machines.
update capacity_requests
set state = 3, message = 'created before machine credentials; retry with a new request_id'
where state in (1, 2);
alter table capacity_requests
  add column credential bytea not null default '',
  add column credential_digest bytea not null default '';
alter table capacity_requests
  alter column credential drop default,
  alter column credential_digest drop default;
