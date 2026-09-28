-- name: UpsertTask :one
insert into tasks (
  id, capability, domain_tags, difficulty_tier, difficulty_knobs, prompt,
  attachments, temperature, max_tokens, grader_specs, dimension_weights,
  pass_threshold, smoke_member, corpus_version, active
)
values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
on conflict (id) do update set
  capability = excluded.capability,
  domain_tags = excluded.domain_tags,
  difficulty_tier = excluded.difficulty_tier,
  difficulty_knobs = excluded.difficulty_knobs,
  prompt = excluded.prompt,
  attachments = excluded.attachments,
  temperature = excluded.temperature,
  max_tokens = excluded.max_tokens,
  grader_specs = excluded.grader_specs,
  dimension_weights = excluded.dimension_weights,
  pass_threshold = excluded.pass_threshold,
  smoke_member = excluded.smoke_member,
  corpus_version = excluded.corpus_version,
  active = excluded.active,
  updated_at = now()
returning *;

-- name: DeactivateCorpusTasksNotIn :execrows
update tasks
set active = false, updated_at = now()
where corpus_version = @corpus_version
  and not (id = any(@task_ids::text[]));

-- name: GetTask :one
select * from tasks
where id = $1;

-- name: ListTasksByCorpusVersion :many
select * from tasks
where corpus_version = $1
order by id;

-- name: ListActiveTasks :many
select * from tasks
where active
order by id;

-- name: ListActiveTasksByCorpusVersion :many
select * from tasks
where active and corpus_version = $1
order by id;
