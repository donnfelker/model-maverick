# Deploy images, Kamal, and backups

Bead `model-maverick-rust-port-3hk.9.4`. Master plan §4 "Deployment" and spec §14.2 are the
authority. The go-live runbook (bead 9.5) uses these files on the real host.

## The images

`docker/Dockerfile` builds both images from one builder stage:

```sh
docker build -f docker/Dockerfile --target engine -t maverick .
docker build -f docker/Dockerfile --target proxy  -t maverick-proxy .
```

- cargo-chef cooks the dependencies in a layer of their own, so a source change rebuilds
  only the workspace crates.
- The binaries are static musl builds. The build fails if a binary has an ELF interpreter.
- The final images are `gcr.io/distroless/static-debian12:nonroot`: no libc, no shell, and
  the nonroot user.
- TLS is rustls only. The builder has no OpenSSL headers.
- The checked queries compile from `.sqlx/` (`SQLX_OFFLINE`). Run `scripts/sqlx-prepare.sh`
  after a query change.
- `.dockerignore` sends only `Cargo.toml`, `Cargo.lock`, `crates/`, and `.sqlx/`.

The engine image runs every process shape through its arguments: `serve` (the default),
`serve --api-only`, `serve --worker-only`, and `migrate`. The proxy image runs
`maverick-proxy`, configured by its environment (reference part 6 §2).

## The local smoke

```sh
scripts/docker-smoke.sh                     # builds both images, then runs the smoke
SMOKE_SKIP_BUILD=1 scripts/docker-smoke.sh  # reuses maverick:smoke and maverick-proxy:smoke
```

It runs Postgres 16, the engine, the hosted proxy, and a fake OpenAI upstream on a private
Docker network. It checks the migration, `/up`, `/health/detail`, one proxy request, and the
backup and restore test.
- No real provider is called.
- **Every published port binds 127.0.0.1.** On this kind of host, Docker-published ports
  bypass the firewall, so a bare `-p 8080:8080` would be public. Keep the `127.0.0.1::` form
  in any script you add.

## Kamal 2

- `config/deploy.yml`: the engine.
  - `web` runs `serve --api-only` behind kamal-proxy.
  - One `worker` runs `serve --worker-only`.
  - Accessories: Postgres 16, and the nightly `backup`.
  - `.kamal/hooks/pre-app-boot` runs `maverick migrate` with the new image before the new
    containers boot.
- `config/deploy-proxy.yml`: the hosted trial proxy, its own service (`kamal deploy -c
  config/deploy-proxy.yml`). It has no MM key and no provider key (D12, D13).
- `.kamal/secrets`: environment references only. Export the values in the deploying shell.

Replace the placeholders (`<HOST>`, `<REGISTRY_USERNAME>`, `maverick.example.com`,
`proxy.example.com`) before `kamal setup`. `kamal config` and `kamal config -c
config/deploy-proxy.yml` print the resolved configuration without contacting a host.

The internal listener (:8081) is never routed. The web container publishes it on the host
loopback only; reach it over SSH (`ssh -N -L 8081:127.0.0.1:8081 <user>@<HOST>`).

## Backups

`scripts/backup.sh` (POSIX sh) dumps the database with `pg_dump --format=custom`. Next to
each dump it writes the exact row count of every public table, counted in the dump's own
snapshot.
- The `backup` accessory runs it with `--daemon` every night at 02:30 UTC, into the
  `maverick-backups` volume. It keeps 7 dumps.
- **Off the box:** set `MAVERICK_BACKUP_TARGET` to an rclone remote path (for example
  `offsite:maverick`) and the `RCLONE_CONFIG_OFFSITE_*` secrets. Unset, every run logs a
  warning, because the dump then exists on the host only.
- The capture master keys and the internal tokens are not in the database. Keep them in the
  password manager with the other secrets (docs/operations.md "Backups").

`scripts/restore-test.sh` restores a dump into a scratch database and runs two checks. Every
table's row count must equal the `.counts` file, and `maverick migrate` must succeed on the
result. It drops the scratch database afterwards. Run it with client tools of the server's
major version, for example on the host:

```sh
docker cp "$(docker create <REGISTRY_USERNAME>/maverick:<version>)":/usr/local/bin/maverick ./maverick
docker run --rm --network kamal \
  -v maverick-backups:/backups -v "$PWD/scripts:/scripts:ro" -v "$PWD/maverick:/usr/local/bin/maverick:ro" \
  -e MAVERICK_BACKUP_DIR=/backups -e MAVERICK_DATABASE_URL -e MAVERICK_INTERNAL_TOKENS \
  -e MAVERICK_CAPTURE_MASTER_KEY postgres:16-alpine sh /scripts/restore-test.sh
```
