-- Login wakes counted in the current wake window that no report with players online has confirmed yet.
alter table environments add column wake_logins_pending integer not null default 0;
