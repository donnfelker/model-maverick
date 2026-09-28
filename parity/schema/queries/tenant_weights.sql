-- name: CreateTenantWeight :one
insert into tenant_weights (tenant_id, capability, w_quality, w_cost, w_latency, quality_bar)
values ($1, $2, $3, $4, $5, $6)
returning *;

-- name: ListTenantWeightsByTenant :many
select * from tenant_weights
where tenant_id = $1
order by capability;

-- name: DeleteTenantWeightsByTenant :exec
delete from tenant_weights
where tenant_id = $1;
