-- name: UpsertCorpusVersion :one
insert into corpus_versions (version, description, task_count, active)
values ($1, $2, $3, $4)
on conflict (version) do update set
  description = excluded.description,
  task_count = excluded.task_count
returning *;

-- name: GetCorpusVersion :one
select * from corpus_versions
where version = $1;

-- name: GetActiveCorpusVersion :one
select * from corpus_versions
where active
limit 1;

-- name: ListCorpusVersions :many
select * from corpus_versions
order by created_at, version;

-- name: AcquireCorpusActivationLock :exec
select pg_advisory_xact_lock($1);

-- name: ActivateCorpusVersion :execrows
update corpus_versions
set active = (corpus_versions.version = $1)
where (corpus_versions.active or corpus_versions.version = $1)
  and exists (select 1 from corpus_versions cv where cv.version = $1);
