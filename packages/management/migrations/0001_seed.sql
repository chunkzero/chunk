-- The single rows `installation` and `reconciler_leader` hold.
insert into installation (id) values (gen_random_uuid()::text);
--> statement-breakpoint
insert into reconciler_leader (epoch) values (0);
