-- Tenant telemetry opt-out (spec v1 §9.10, D39). When true, recommend
-- responses carry telemetry: false, the proxy and SDK cores send nothing for
-- the tenant, and the engine drops any items it still receives without writing
-- outcomes rows. It is not a column filter, so G4 simulation ignores it.

alter table tenant_constraints
  add column telemetry_opt_out boolean not null default false;
