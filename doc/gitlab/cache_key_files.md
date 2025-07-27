##### `cache:key:files`

Use the `cache:key:files` keyword to generate a new key when files matching either of the defined paths or patterns
change. `cache:key:files` lets you reuse some caches, and rebuild them less often,
which speeds up subsequent pipeline runs.

**Keyword type**: Job keyword. You can use it only as part of a job or in the
[`default` section](#default).

**Supported values**:

- An array of up to two file paths or patterns.

CI/CD variables are not supported.

**Example of `cache:key:files`**:

```yaml
cache-job:
  script:
    - echo "This job uses a cache."
  cache:
    key:
      files:
        - Gemfile.lock
        - package.json
    paths:
      - vendor/ruby
      - node_modules
```

This example creates a cache for Ruby and Node.js dependencies. The cache
is tied to the current versions of the `Gemfile.lock` and `package.json` files. When one of
these files changes, a new cache key is computed and a new cache is created. Any future
job runs that use the same `Gemfile.lock` and `package.json` with `cache:key:files`
use the new cache, instead of rebuilding the dependencies.

**Additional details**:

- The cache `key` is a SHA computed from the most recent commits
  that changed each listed file.
  If neither file is changed in any commits, the fallback key is `default`.
- Wildcard patterns like `**/package.json` can be used. An [issue](https://gitlab.com/gitlab-org/gitlab/-/issues/301161)
  exists to increase the number of paths or patterns allowed for a cache key.
