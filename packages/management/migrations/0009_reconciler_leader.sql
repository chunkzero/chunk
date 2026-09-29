-- The reconciler leader's epoch. A process that takes the leader lock bumps it, and every reconciler transaction that
-- acts first shares a lock on the row while checking its epoch is still current: a bump waits for an older leader's
-- open transaction and refuses its later ones.
create table reconciler_leader (epoch bigint not null);
insert into reconciler_leader values (0);

-- Whether a JVM request's machine was ever started; one that was is never started again.
alter table capacity_requests add column started boolean not null default false;
