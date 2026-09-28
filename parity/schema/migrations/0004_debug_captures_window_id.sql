-- +goose Up

-- window_id records which capture window produced a debug_captures row (the
-- operator capture-read surface, spec §9.8/§10.4 extension: GET
-- /internal/v1/captures filters by window_id and returns it per entry).
-- Nullable and `on delete set null` — unlike every other FK in this schema —
-- because DELETE /v1/debug/capture-windows/{id} (spec §9.8 "end early") hard-
-- deletes the window row immediately, while any captures already stored
-- under it keep living out their own expires_at; a plain FK would make that
-- delete fail once a window had captured anything. The hourly capture_expiry
-- sweep never hits this path: it deletes a window only once every capture
-- that could reference it (expires_at = window.ends_at + capture.retention)
-- has already been hard-deleted in the same pass.
alter table debug_captures
  add column window_id text references capture_windows(id) on delete set null;

create index on debug_captures (window_id);
create index on debug_captures (tenant_id);

-- +goose Down

drop index debug_captures_tenant_id_idx;

alter table debug_captures
  drop column window_id;
