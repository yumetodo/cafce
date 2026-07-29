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

## MSRV

The minimum supported Rust version is `1.94.1`, driven by the `aws-sdk-*` dependency tree (a lower bound — newer `rustc` is fine). `rust-toolchain.toml` pins the toolchain to exactly `1.94.1`, so there is currently zero margin on the low side: any setup that resolves to an older `rustc` will fail to compile. `rust-toolchain.toml` only pins the toolchain for builds run from within this directory tree — building with `--manifest-path` from outside it (or any other setup that skips toolchain auto-detection) falls back to whatever `rustc` is otherwise on `PATH`, which can be older and fail to compile.

## Commands

| Command | Description |
|---|---|
| `cafce key <CONFIG>` | Compute the primary cache key from `<CONFIG>` and print it to stdout on a single line. Exit code 0 on success. |
| `cafce probe <CONFIG>` | Check whether the primary key (or any of `fallback_keys`) exists in S3. Prints `true` or `false` to stdout. Exit code 0 for both hit and miss; non-zero on S3 / auth / config errors written to stderr. |
| `cafce init <CONFIG>` | Write a starter TOML configuration file to `<CONFIG>`. |
| `cafce store <CONFIG>` | Archive everything matched by `paths` and upload it under the primary key. Prints `true` when it uploaded, `false` when the cache contents were unchanged and the upload was skipped. Exit code 0 for both. |
| `cafce restore <CONFIG>` | Try the primary key then each of `fallback_keys`, and extract the first object that hits into the current directory. Prints `true` when it extracted something, `false` on a full miss. Exit code 0 for both. |

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

`store` and `restore` follow the same convention, so a whole CI cache cycle composes with shell:

```sh
cafce restore cafce.toml   # prints true (extracted) or false (full miss)
make build
cafce store cafce.toml     # prints true (uploaded) or false (contents unchanged)
```

Detailed progress — how many entries were archived, the archive size, which fallback key was
chosen — goes to `log::info!` / `log::debug!`, not stdout. Use `RUST_LOG=info` to see it.

## Configuration file

A cafce configuration file is a TOML document with the following fields:

| Field | Type | Required | Description |
|---|---|---|---|
| `project` | string | yes | Operational namespace. Becomes a segment of the S3 object key. `${VAR}` env expansion is applied. |
| `key` | string OR table | yes | The primary cache key. Either a literal string, or a `{ files, prefix }` table (see below). |
| `key.files` | array of strings | — | Glob patterns whose contents are hashed to form the key. Patterns are resolved from the current working directory; absolute paths are rejected. |
| `key.prefix` | string | — | Prepended to the computed hash (e.g. `"deps-v1"` becomes `deps-v1-<hash>`). `${VAR}` env expansion is applied. |
| `fallback_keys` | array of strings | — | Alternative cache keys tried in order by `cafce probe` when the primary key misses. Empty by default. `${VAR}` env expansion is applied to each element. |
| `paths` | array of strings | — | Glob patterns naming what `store` puts into the cache. See below. Required (non-empty) for `store`; ignored by `restore`. |

Unknown fields are rejected at parse time (`#[serde(deny_unknown_fields)]`) so that typos surface immediately rather than silently taking effect.

### `${VAR}` env expansion

Environment variable references of the form `${VAR}` are expanded when the config is parsed. Referring to an undefined variable is a hard error. The syntax is intentionally limited to the braced form; bare `$VAR` is not supported.

Expansion is applied to fields that participate in the cache key itself (`project`, `key` as a literal string, `key.prefix`, and each element of `fallback_keys`) but **not** to filesystem paths (`key.files` and `paths`).

### `paths`

`paths` lists what `store` archives. Patterns are resolved against the current working directory,
the same base as `key.files`.

| Item | Behaviour |
|---|---|
| What you can name | Files, directories, and wildcards (`*`, `**`, `?`, `[...]`) |
| Directories | Expanded recursively, including empty directories and the directory entries themselves |
| Symlinks | Stored as links, never followed |
| Absolute paths | Rejected |
| Escaping the base directory (`../`) | Rejected |
| Zero matches | An error for `store` |
| Empty array | An error for `store` |
| Count limit | None. The 50-file cap on `key.files` applies only to key computation |

Zero matches is an error rather than an empty archive: a pattern that is written but matches
nothing usually means a misconfiguration or a failed build step, and storing an empty archive
would make later `restore` calls report a hit while restoring nothing.

`restore` deliberately does **not** read `paths`. What an archive contains is recorded in the
archive itself, so a `paths` change between `store` and `restore` cannot make extraction
behave unpredictably.

**Recommendation:** put build artifacts and dependency caches in `paths` (`target`,
`node_modules`, `~/.cargo` copied into the workspace, …) — not the repository working tree
itself. Restored files get the extraction time as their mtime, so mixing sources and artifacts
in one cache can make mtime-based incremental builds (cargo, make) behave inconsistently.

### Examples

Literal-string key:

```toml
project = "my-app"
key = "cache-${CI_COMMIT_REF_SLUG}"
fallback_keys = ["cache-${CI_DEFAULT_BRANCH}", "cache-default"]
paths = ["target"]
```

Files-based key (table form). Because everything after `[key]` becomes a child of that table, top-level scalars and arrays must come **before** the `[key]` heading:

```toml
project = "my-app"
fallback_keys = []
paths = ["target", "node_modules"]

[key]
files = ["Cargo.lock", "package.json"]
prefix = "deps-v1"
```

Files-based key (inline table form):

```toml
project = "my-app"
key = { files = ["Cargo.lock"], prefix = "deps-v1" }
fallback_keys = []
paths = ["target"]
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
| `CAFCE_S3_CHECKSUM` | — | How to use S3 flexible checksums: `auto` (default), `off`, `required`. See below. |

`RUST_LOG` (e.g. `RUST_LOG=info`) controls developer tracing via `env_logger`. `store` and
`restore` report entry counts, archive size, the chosen object key, and checksum fallbacks there.

`TMPDIR` matters for `store` and `restore`: both stream the archive through a temporary file
rather than holding it in memory, using the platform default location (`tempfile` semantics).
A CI runner with a small or tmpfs-backed `/tmp` can fail on large caches; point `TMPDIR` at a
disk with room for one full cache.

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

The object body is a **deterministic tar archive compressed with zstd** (`tar+zstd`). Identical
inputs produce an identical byte stream regardless of when, where, or in what order they were
enumerated: entry order, mtime, uid/gid, and permissions are all normalized. That is what makes
"the contents did not change, so skip the upload" a reliable decision rather than a guess.

Determinism is guaranteed **within one platform**. Windows cannot report the executable bit, so a
file that is `0o755` on Unix becomes `0o644` there and the hash differs. The only consequence is a
redundant re-upload when runners of mixed OSes share a bucket; correctness is unaffected.

`project` is an **operational** namespace, not a security boundary: any principal with write access to the bucket can write under any `{project}` value. For real multi-tenant isolation, use separate buckets, or restrict IAM policies to a specific prefix (e.g. `arn:aws:s3:::my-bucket/prefix/project/*`).

## S3 object metadata

`store` attaches three user metadata entries to every object it uploads. S3 sends them with an
`x-amz-meta-` prefix and returns them lowercased; the names below omit that prefix.

| Key | Value format | Meaning |
|---|---|---|
| `cafce-schema-version` | Decimal integer string. Currently `1` | Schema version of the metadata and archive layout |
| `cafce-archive-format` | `tar+zstd` | Archive format of the payload |
| `cafce-content-sha256` | Lowercase hex, 64 characters | SHA-256 of the **uncompressed tar stream** — the identity of the cache contents |

`cafce-content-sha256` hashes the tar stream rather than the object body on purpose. A different
zstd version or compression level changes the body bytes but not the tar stream, so a fleet
running mixed cafce versions does not re-upload identical content back and forth.

Reading behaviour:

- An object with an **unknown (future) schema version** is not extracted; `restore` fails loudly
  rather than guessing at a layout it does not understand.
- An object with **no cafce metadata at all** (uploaded by `aws s3 cp`, or by another tool) is
  still extracted by `restore`, with the content hash check skipped and a warning logged.
  `store` treats it the same as a hash mismatch and overwrites it.

### Integrity checking

Two independent hashes with different jobs:

| | `x-amz-checksum-sha256` | `x-amz-meta-cafce-content-sha256` |
|---|---|---|
| Covers | The object body (the tar.zst bytes) | The uncompressed tar stream |
| Format | Base64 of the 32-byte digest | Lowercase hex, 64 characters |
| Purpose | Detecting transfer / storage corruption | Deciding whether contents are identical |
| Verified by | S3 on upload, the AWS SDK on download | cafce itself |

`store` sends a **pre-computed** `x-amz-checksum-sha256` so S3 recomputes and compares it
server-side, rejecting a corrupted upload with `BadDigest`. `restore` asks the SDK to verify it
while the body is read. Because cafce writes the body to a temporary file before extracting,
a transfer corruption is caught *before* the working directory is touched.

Flexible checksums are an AWS extension and S3-compatible servers vary in their support, so
`CAFCE_S3_CHECKSUM` selects the policy:

| Value | Behaviour |
|---|---|
| `auto` (default) | Best effort. If an upload fails in a way that looks like the server does not support checksums, log a warning and retry once without one. `BadDigest` is never retried — it means real corruption. |
| `off` | Never send or request a checksum. |
| `required` | Never fall back. An upload failure is an error, and `restore` fails if the server returns no checksum. Use this on real AWS S3 to guarantee verification is not silently disabled. |

Correctness never depends on this: the metadata content hash verifies the payload independently.
Flexible checksums only move detection earlier and make it more certain.

## Required IAM permissions

Grant the following actions on the bucket configured via `CAFCE_AWS_BUCKET`:

| Action | Why |
|---|---|
| `s3:GetObject` | Required to check individual cache objects (`probe`) and to download them (`restore`). |
| `s3:PutObject` | Required by `store` to upload cache objects. |
| `s3:ListBucket` | Required so that AWS S3 returns `404 NotFound` (not `403 AccessDenied`) for missing keys. cafce treats 403 as an error — not a cache miss — because a silent auth failure disguised as a permanent cache miss would cause every CI run to fall back to a full build without any visible warning. |

cafce never deletes cache objects, so `s3:DeleteObject` is not needed. Expiry and generation
management are left to S3 lifecycle policies.

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
mkdir -p build
echo artifact > build/app

cat > cafce.toml <<'EOF'
project = "cafce-dev"
key = "hello"
fallback_keys = []
paths = ["build"]
EOF
```

Then run a whole cache cycle:

```sh
cafce key     cafce.toml   # prints: hello
cafce probe   cafce.toml   # prints: false  (nothing has been stored yet)
cafce store   cafce.toml   # prints: true   (uploads cafce-dev/cafce-dev/hello)
cafce store   cafce.toml   # prints: false  (contents unchanged, upload skipped)
cafce probe   cafce.toml   # prints: true
rm -rf build
cafce restore cafce.toml   # prints: true   (build/app is back)
```
