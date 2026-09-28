-- name: CreatePool :one
insert into pools (id, tenant_id, name, model_column_ids, is_default)
values ($1, $2, $3, $4, $5)
returning *;

-- name: ListPoolsByTenant :many
select * from pools
where tenant_id = $1
order by name;

-- name: ListPools :many
select * from pools
order by tenant_id, name;

-- name: ListPoolsByColumn :many
select * from pools
where @column_id::text = any(model_column_ids)
order by tenant_id, name;

-- name: DeletePoolsByTenant :exec
delete from pools
where tenant_id = $1;
