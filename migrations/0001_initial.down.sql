-- Reverts 0001_initial: drops the initial schema, children before the tables they reference.
DROP TABLE kv;
DROP TABLE port_allocations;
DROP TABLE service_ports;
DROP TABLE services;
DROP INDEX deployment_events_by_deployment;
DROP TABLE deployment_events;
DROP TABLE deployment_targets;
DROP INDEX deployments_unfinished;
DROP INDEX deployments_by_stack;
DROP TABLE deployments;
DROP TABLE stack_revisions;
DROP TABLE stacks;
