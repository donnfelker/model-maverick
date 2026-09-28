-- name: GetTenantConstraints :one
select * from tenant_constraints
where tenant_id = $1;

-- name: UpsertTenantConstraints :one
insert into tenant_constraints (tenant_id, max_cost_per_run, max_latency_ms, zero_retention)
values ($1, $2, $3, $4)
on conflict (tenant_id) do update set
  max_cost_per_run = excluded.max_cost_per_run,
  max_latency_ms = excluded.max_latency_ms,
  zero_retention = excluded.zero_retention,
  updated_at = now()
returning *;
