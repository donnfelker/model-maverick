-- name: GetClassificationCache :one
select * from classification_cache
where tenant_id = $1
  and prompt_hash = $2
  and classifier_version = $3;

-- name: UpsertClassificationCache :exec
insert into classification_cache (tenant_id, prompt_hash, classifier_version, classification)
values ($1, $2, $3, $4)
on conflict (tenant_id, prompt_hash, classifier_version) do nothing;
