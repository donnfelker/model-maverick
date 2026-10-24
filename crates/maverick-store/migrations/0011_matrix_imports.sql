-- D49: bundle metadata is separate from the origin's private corpus versions.
create table matrix_imports (
    manifest_sha256 bytea primary key,
    format text not null,
    matrix_version text not null,
    published_at timestamptz not null,
    origin_release text not null,
    signing_kids text[] not null,
    active boolean not null default false,
    created_at timestamptz not null default now()
);

create unique index matrix_imports_one_active on matrix_imports (active) where active;
