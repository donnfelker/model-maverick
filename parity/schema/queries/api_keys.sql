-- name: CreateAPIKey :one
insert into api_keys (id, tenant_id, key_hash, last4, name)
values ($1, $2, $3, $4, $5)
returning *;

-- name: GetAPIKey :one
select * from api_keys
where id = $1;

-- name: GetAPIKeyByHash :one
select * from api_keys
where key_hash = $1;

-- name: ListAPIKeysByTenant :many
select * from api_keys
where tenant_id = $1
order by id;

-- name: RevokeAPIKey :one
update api_keys
set status = 'revoked',
    revoked_at = now()
where id = $1
returning *;

-- name: DeleteAPIKey :exec
delete from api_keys
where id = $1;
