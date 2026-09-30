-- Whether the current wake window holds a login wake that no report with a player online has confirmed yet.
alter table environments add column wake_refundable boolean not null default false;
