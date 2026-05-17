# concord

A Discord-like chat application, implemented as a Rust workspace.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the workspace layout,
runtime topology, and database schema (including the ER diagram).

## Getting started

1. Copy environment defaults:

   ```sh
   cp .env.example .env
   # then edit .env to set real passwords / secrets
   ```

2. Start Postgres and Redis:

   ```sh
   docker compose up -d
   ```

## Database migrations

Migrations live in `/migrations` and are managed with
[`sqlx-cli`](https://crates.io/crates/sqlx-cli).

Install the CLI once:

```sh
cargo install sqlx-cli --no-default-features --features rustls,postgres
```

Uncomment `DATABASE_URL` in `.env` (or export it in your shell) so it points
at the running Postgres, then:

> **Note:** if your password contains `@`, `:`, `/`, `?`, `#`, or other
> reserved URL characters, percent-encode them in the connection string
> (e.g. `@` → `%40`). Otherwise the URL parser will misinterpret them and
> the connection will fail with a confusing error.

```sh
export DATABASE_URL="postgres://concord:<POSTGRES_PASSWORD>@localhost:5432/concord"

sqlx database create        # first time only
sqlx migrate run            # apply all pending migrations
```

To start from a clean slate during development:

```sh
sqlx database drop -y && sqlx database create && sqlx migrate run
```

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md#4-data-layer) for the
schema and the cascade-delete policy.
