-- Identifies this install to the provider, which labels every machine and volume it creates with it.
create table installation (id text primary key);
insert into installation (id) values (gen_random_uuid()::text);
