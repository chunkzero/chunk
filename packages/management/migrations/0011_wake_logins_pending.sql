-- Login wakes counted in the current wake window that no login reported since has confirmed yet.
alter table environments add column wake_logins_pending integer not null default 0;
-- The login count of the latest accepted report from the lease owner's core instance.
alter table environments add column report_logins bigint not null default 0;
