-- batches: the engine-owned uniqueness and lease record for every job
-- (spec v1 §3 and §11.2, D10). One row per job kind and schedule window. The id
-- is deterministic per (kind, batch_window) and equals runs.batch_id for batch
-- kinds. The column is batch_window because window is a reserved word.

create table batches (
  id                text primary key,
  kind              text not null,
  batch_window      text not null,
  status            text not null default 'running'
                    check (status in ('running','completed','skipped')),
  lease_owner       text,
  lease_expires_at  timestamptz,
  heartbeat_at      timestamptz,
  attempt           int not null default 1,
  completed_at      timestamptz,
  created_at        timestamptz not null default now(),
  updated_at        timestamptz not null default now(),
  unique (kind, batch_window)
);
