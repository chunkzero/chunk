-- The Java version a JVM request's release runs on, which picks its machine's runner image; null for gateways.
alter table capacity_requests add column java_version integer;
