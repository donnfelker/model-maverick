-- name: CreateTenant :one
insert into tenants (id, name, prompt_hmac_key)
values ($1, $2, $3)
returning *;

-- name: GetTenant :one
select * from tenants
where id = $1;

-- name: ListTenants :many
select * from tenants
order by id;

-- name: UpdateTenant :one
update tenants
set name = $2,
    status = $3,
    updated_at = now()
where id = $1
returning *;

-- name: DeleteTenant :exec
delete from tenants
where id = $1;
