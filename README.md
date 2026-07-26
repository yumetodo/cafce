# Cache restore/store AFter git Checkout/ci Execution

`cafce` provides CI job side cache key calculation and cache management for S3 compatible API server.

## Motivation

Caching in GitLab CI/CD has a problem that cache restoring has occurred before executing git checkout.

This problem leads the limitation that only 2 blob expression is allowed for cache key file.

> https://gitlab.com/gitlab-org/gitlab/-/merge_requests/120433#note_1388606101
>
> @marcel.amirault yup, this low limit was put in place for performance reasons: [#18986 (comment 229934886)](https://gitlab.com/gitlab-org/gitlab/-/issues/18986#note_229934886)
> I think we need to work on the Gitaly side first to remove the [N+1 requests](https://docs.gitlab.com/ee/development/gitaly.html#toomanyinvocationserror-errors) before increasing the limit since this could generate 40 Gitaly calls per job if using [multiple caches](https://docs.gitlab.com/ee/ci/caching/index.html#use-multiple-caches) times the number of jobs in the pipeline that use caching and 😬 :
>
> > The GitalyClient attempts to block against potential n+1 issues by raising this error when Gitaly is called more than 30 times in a single Rails request or Sidekiq execution.

According to this GitLab CI/CD maintainer's post, calculating the cache key from files is executed on the GitLab server.  And the limitation comes from the server-side performance issue.

So, there is a simple solution that calculates the cache key on each CI job after executing git checkout.

## Commands

| Command | Description |
|---|---|
| `cafce key <CONFIG>` | Compute the primary cache key from `<CONFIG>` and print it to stdout on a single line. Exit code 0 on success. |
| `cafce probe <CONFIG>` | Check whether the primary key (or any of `fallback_keys`) exists in S3. Prints `true` or `false` to stdout. Exit code 0 for both hit and miss; non-zero on S3 / auth / config errors written to stderr. |
| `cafce init <CONFIG>` | Write a starter TOML configuration file to `<CONFIG>`. |
| `cafce store <CONFIG>` | Not yet implemented — reserved for [#7](https://github.com/yumetodo/cafce/issues/7). |
| `cafce restore <CONFIG>` | Not yet implemented — reserved for [#7](https://github.com/yumetodo/cafce/issues/7). |

`cafce key` is intended to be composed with shell, for example:

```sh
KEY=$(cafce key cafce.toml)
```

`cafce probe` is intended to be used as a predicate in CI scripts:

```sh
if [ "$(cafce probe cafce.toml)" = "true" ]; then
    echo "cache hit"
fi
```

Because `probe` speaks only `true` / `false` on stdout, use `cafce key` when you need to see the computed key string for debugging.

## Configuration file

A cafce configuration file is a TOML document with the following fields:

| Field | Type | Required | Description |
|---|---|---|---|
| `project` | string | yes | Operational namespace. Becomes a segment of the S3 object key. `${VAR}` env expansion is applied. |
| `key` | string OR table | yes | The primary cache key. Either a literal string, or a `{ files, prefix }` table (see below). |
| `key.files` | array of strings | — | Glob patterns whose contents are hashed to form the key. Patterns are resolved from the current working directory; absolute paths are rejected. |
| `key.prefix` | string | — | Prepended to the computed hash (e.g. `"deps-v1"` becomes `deps-v1-<hash>`). `${VAR}` env expansion is applied. |
| `fallback_keys` | array of strings | — | Alternative cache keys tried in order by `cafce probe` when the primary key misses. Empty by default. `${VAR}` env expansion is applied to each element. |
| `paths` | array of strings | — | Reserved for [#7](https://github.com/yumetodo/cafce/issues/7) (`store` / `restore`). Currently parsed but unused. |

Unknown fields are rejected at parse time (`#[serde(deny_unknown_fields)]`) so that typos surface immediately rather than silently taking effect.

### `${VAR}` env expansion

Environment variable references of the form `${VAR}` are expanded when the config is parsed. Referring to an undefined variable is a hard error. The syntax is intentionally limited to the braced form; bare `$VAR` is not supported.

Expansion is applied to fields that participate in the cache key itself (`project`, `key` as a literal string, `key.prefix`, and each element of `fallback_keys`) but **not** to filesystem paths (`key.files` and `paths`).

### Examples

Literal-string key:

```toml
project = "my-app"
key = "cache-${CI_COMMIT_REF_SLUG}"
fallback_keys = ["cache-${CI_DEFAULT_BRANCH}", "cache-default"]
paths = []
```

Files-based key (table form). Because everything after `[key]` becomes a child of that table, top-level scalars and arrays must come **before** the `[key]` heading:

```toml
project = "my-app"
fallback_keys = []
paths = []

[key]
files = ["Cargo.lock", "package.json"]
prefix = "deps-v1"
```

Files-based key (inline table form):

```toml
project = "my-app"
key = { files = ["Cargo.lock"], prefix = "deps-v1" }
fallback_keys = []
paths = []
```

A minimal working example is checked in at [`test/sample/setting.toml`](test/sample/setting.toml).

## Environment variables

| Variable | Required | Description |
|---|---|---|
| `CAFCE_AWS_BUCKET` | yes | S3 bucket that holds the cache objects. |
| `CAFCE_S3_PREFIX` | — | Prefix prepended to the S3 object key. Trailing slashes are normalized away. |
| `CAFCE_AWS_SERVER_ADDRESS` | — | S3-compatible endpoint (e.g. `localhost:9000`, `10.200.1.157:9000`). Omit for AWS S3. |
| `CAFCE_AWS_ACCESS_KEY` | — | Static access key (used with RustFS / MinIO or as the source credential for AssumeRole). |
| `CAFCE_AWS_SECRET_KEY` | — | Static secret key. |
| `CAFCE_AWS_SESSION_TOKEN` | — | Session token for temporary credentials. |
| `CAFCE_AWS_ROLE_ARN` | — | ARN of a role to assume; static access/secret keys are used as the source credential. |
| `CAFCE_AWS_ROLE_SESSION_NAME` | — | Session name used when assuming the role above. Defaults to `cafce-session`. |
| `CAFCE_AWS_PROFILE` | — | Named profile from `~/.aws/config` to use as the credential provider. |
| `CAFCE_AWS_INSECURE` | — | `true` to use plain HTTP (for local RustFS / MinIO). Defaults to `false`. |
| `CAFCE_AWS_REGION` | — | AWS region. Defaults to `us-east-1`. |
| `CAFCE_AWS_FORCE_PATH_STYLE` | — | Override the auto-detected addressing style (`true` / `false`). Auto-detection uses path-style for non-`amazonaws.com` endpoints. |

`RUST_LOG` (e.g. `RUST_LOG=debug`) controls developer tracing via `env_logger`. The infrastructure is wired up, but cafce does not yet emit tracing calls, so this is currently a no-op.

## S3 object key layout

cafce stores each cache object at:

```
{bucket}/{prefix?}/{project}/{cache_key}
```

- `{bucket}` — `CAFCE_AWS_BUCKET`
- `{prefix?}` — `CAFCE_S3_PREFIX` when set, otherwise omitted
- `{project}` — `Setting.project` from the config file
- `{cache_key}` — the value printed by `cafce key`

Multiple repositories can share a single bucket safely because the `{project}` segment gives each one its own namespace.

`project` is an **operational** namespace, not a security boundary: any principal with write access to the bucket can write under any `{project}` value. For real multi-tenant isolation, use separate buckets, or restrict IAM policies to a specific prefix (e.g. `arn:aws:s3:::my-bucket/prefix/project/*`).

## Required IAM permissions

Grant the following actions on the bucket configured via `CAFCE_AWS_BUCKET`:

| Action | Why |
|---|---|
| `s3:GetObject` | Required to check individual cache objects (used by `probe`, and by `restore` once [#7](https://github.com/yumetodo/cafce/issues/7) lands). |
| `s3:ListBucket` | Required so that AWS S3 returns `404 NotFound` (not `403 AccessDenied`) for missing keys. cafce treats 403 as an error — not a cache miss — because a silent auth failure disguised as a permanent cache miss would cause every CI run to fall back to a full build without any visible warning. |

Once [#7](https://github.com/yumetodo/cafce/issues/7) adds `store` / `restore`, `s3:PutObject` and `s3:DeleteObject` will also be needed.

## Local development (RustFS)

`docker-compose.yml` at the repo root starts a local [RustFS](https://github.com/rustfs/rustfs) instance (S3-compatible) for testing cafce's S3 client against:

```sh
docker compose up -d
```

Then point cafce at it via (see `src/env.rs`):

```sh
export CAFCE_AWS_SERVER_ADDRESS=localhost:9000
export CAFCE_AWS_ACCESS_KEY=cafce-dev-access-key
export CAFCE_AWS_SECRET_KEY=cafce-dev-secret-key
export CAFCE_AWS_INSECURE=true
export CAFCE_AWS_BUCKET=cafce-dev
```

RustFS does not pre-create the bucket referenced by `CAFCE_AWS_BUCKET`; create it once via the AWS CLI (or any other S3 client):

```sh
aws --endpoint-url http://localhost:9000 s3 mb s3://cafce-dev
```

### End-to-end smoke test

With the exports above in place, drop a minimal config into the current directory:

```sh
cat > cafce.toml <<'EOF'
project = "cafce-dev"
key = "hello"
fallback_keys = []
paths = []
EOF
```

Then compute the key and probe for it:

```sh
cafce key   cafce.toml   # prints: hello
cafce probe cafce.toml   # prints: false  (nothing has been stored yet)
```

`probe` will start returning `true` once `store` (planned for [#7](https://github.com/yumetodo/cafce/issues/7)) or an equivalent `aws s3 cp` uploads an object at `cafce-dev/cafce-dev/hello`.
