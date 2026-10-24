# Deploy images, Kamal, backups, and releases

Bead `model-maverick-rust-port-3hk.9.4`. Master plan §4 "Deployment" and spec §14.2 are the
authority. The go-live runbook, `docs/runbook-go-live.md` (bead 9.5), uses these files on the
real host.

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

The engine requires `MAVERICK_ROLE=origin` or `MAVERICK_ROLE=replica` (D49).
The hosted Kamal deployment and the local smoke set `origin`. A replica runs
maintenance and latency probes, with no corpus benchmarks or model discovery.

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
  - `.kamal/hooks/pre-app-boot` runs `maverick upgrade --yes` with the new image before the
    new containers boot (D50).
- `config/deploy-proxy.yml`: the hosted proxy, its own service (`kamal deploy -c
  config/deploy-proxy.yml`). It has no MM key and no provider key (D12, D13). It can run
  more than one replica: list more hosts. Each replica keeps its own in-memory last good
  store for fail-open (D46), so a new replica starts cold.
- `.kamal/secrets`: environment references only. Export the values in the deploying shell.

Replace the placeholders (`<HOST>`, `<REGISTRY_USERNAME>`, `maverick.example.com`,
`proxy.example.com`) before `kamal setup`. `kamal config` and `kamal config -c
config/deploy-proxy.yml` print the resolved configuration without contacting a host.

The internal listener (:8081) is never routed, and no app role publishes a host port.
Kamal boots the new container before it stops the old one, so a fixed host port would clash
on every deploy (bead 9.13). Reach the listener over SSH with `scripts/internal-tunnel.sh
<user>@<HOST>`. It looks up the running web container's address on the `kamal` network and
forwards `127.0.0.1:8081` on your machine to it. Run it again after each deploy.

## Backups

`scripts/backup.sh` (POSIX sh) dumps the database with `pg_dump --format=custom`. Next to
each dump it writes the exact row count of every public table, counted in the dump's own
snapshot.
- The `backup` accessory runs it with `--daemon` every night at 02:30 UTC. It writes into
  the host directory `~/maverick-backup/maverick-backups` (mounted at `/backups`), and it
  keeps 7 dumps. Kamal mounts an accessory's `directories` from
  `$PWD/<service>-<accessory>/` in the SSH user's home. They are not named volumes.
  Postgres keeps its data the same way, in `~/maverick-postgres/postgres-data`.
- **Off the box:** set `MAVERICK_BACKUP_TARGET` to an rclone remote path (for example
  `offsite:maverick`) and the `RCLONE_CONFIG_OFFSITE_*` secrets. Unset, every run logs a
  warning, because the dump then exists on the host only.
- The capture master keys and the internal tokens are not in the database. Keep them in the
  password manager with the other secrets (docs/operations.md "Backups").

`scripts/restore-test.sh` restores a dump into a scratch database and runs two checks. Every
table's row count must equal the `.counts` file, and `maverick migrate` must succeed on the
result. It drops the scratch database afterwards. Run it with client tools of the server's
major version. `docs/runbook-go-live.md` section 4 has the exact commands for the host.
They copy `maverick` out of the deployed image, and they take the secrets from the web
role's Kamal env file. Then they run the script in `postgres:16-alpine` on the `kamal`
network, with the backup directory mounted at `/backups`.

## Releases

Bead `model-maverick-xq7v.21`. Spec §14.4 "Release artifacts" is the authority.
`.github/workflows/release.yml` runs `scripts/release-artifacts.sh`, which anyone can also run
locally. The script publishes nothing.

```sh
scripts/release-artifacts.sh 0.1.0 dist   # the version must equal the workspace version
```

It writes these files to `dist/`:

- `maverick-<v>-x86_64-unknown-linux-musl.tar.gz`: the static `maverick` and
  `maverick-proxy` binaries, `LICENSE`, and `README.md`;
- `maverick-<v>.cdx.json` and `maverick-proxy-<v>.cdx.json`: CycloneDX 1.5 SBOMs;
- `cargo-deny-<v>.json`: the `cargo deny check` report. A failing check stops the release;
- `maverick-offline-kit-<v>.tar`: both image tarballs (`docker load`), the engine
  migrations, and their `SHA256SUMS`;
- `SHA256SUMS` over every file above.

`maverick --version` in the binaries reports the release date (`MAVERICK_RELEASE_DATE`,
default today in UTC) and the commit (`MAVERICK_RELEASE_COMMIT`, default `HEAD`). Without
`MAVERICK_RELEASE_COMMIT`, the script refuses a checkout with uncommitted build inputs. It
checks the version, date, and commit in the tarball's binary, and that the engine image runs
the same binary. It leaves the images tagged `maverick:<v>` and `maverick-proxy:<v>`.

**Cutting a release:**

1. Set `version` in the root `Cargo.toml` (`[workspace.package]`) to the new SemVer
   version. Commit it, and wait for CI to pass on that commit.
2. Push the tag `v<version>` for that commit. The workflow refuses a tag that differs from
   the workspace version.
3. The workflow pushes `ghcr.io/<owner>/maverick:<v>` and
   `ghcr.io/<owner>/maverick-proxy:<v>`. It signs both images by digest and every file in
   `dist/` with cosign keyless signing (the workflow's GitHub OIDC identity; no key is
   stored). Then it creates the GitHub release with the files and their `.sigstore.json`
   bundles. The release notes list the image digests.

A manual run of the workflow (`workflow_dispatch`) is a dry run: it attaches the unsigned
files to the run and pushes nothing.

**Verifying** (the identity is the release workflow at a tag):

```sh
cosign verify ghcr.io/<owner>/maverick@sha256:<digest> \
  --certificate-identity-regexp '^https://github.com/<owner>/model-maverick/\.github/workflows/release\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
cosign verify-blob --bundle maverick-<v>-x86_64-unknown-linux-musl.tar.gz.sigstore.json \
  --certificate-identity-regexp '^https://github.com/<owner>/model-maverick/\.github/workflows/release\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  maverick-<v>-x86_64-unknown-linux-musl.tar.gz
sha256sum -c SHA256SUMS
```

The images are built for `linux/amd64` only. The registry, the image names, and the
platforms wait for the owner (bead `model-maverick-xq7v.24`).
