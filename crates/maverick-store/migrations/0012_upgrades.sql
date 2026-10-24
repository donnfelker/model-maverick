-- class: online-safe
-- tables: upgrades, serve_heartbeats
--
-- D50 (spec §3, §14.5): the upgrade history that `maverick upgrade` records, and one
-- heartbeat row per running `serve` process, so an upgrade can see which releases run.
create table upgrades (
  id             text primary key,
  from_release   text,
  to_release     text not null,
  operator       text not null,
  migrations     text[] not null default '{}',
  started_at     timestamptz not null,
  finished_at    timestamptz,
  verify_failed  text,
  created_at     timestamptz not null default now()
);

create table serve_heartbeats (
  process_id    text primary key,
  release       text not null,
  host_label    text not null,
  last_seen_at  timestamptz not null,
  created_at    timestamptz not null default now()
);
