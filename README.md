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
