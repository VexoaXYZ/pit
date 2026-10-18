<img src="assets/icon.svg" width="64" height="64" alt="">

# Pit

**CI that is never cold.** Pit is an open-source CI orchestrator for GitHub repos.
GitHub stays the source of truth; Pit replaces Actions runners and the merge queue
with warm workers, content-addressed caching, and grouped execution. One Rust binary.
Whoever runs it hosts it.

```
$ pit run install electron check knip typecheck build test-web     # in T3 Code, warm
pit run_be9e34b3d173 t3code@339179f50a +2 changed file(s) http://127.0.0.1:7070/runs/run_be9e34b3d173
  ci on local
    ◆ install                  cached
    ◆ electron                 cached
    ◆ test-web                 cached
    ◆ knip                     cached
    ◆ check                    cached
    ◆ build                    cached
    ◆ typecheck                cached
  ci workspace ready in 348ms
passed in 1.0s · 0 ran, 7 cached
```

The same pipeline takes ~190s (p50) on GitHub Actions with Blacksmith runners. See
[bench/](bench/README.md) for the full numbers, including where a single machine loses.

## Why

Most CI time is not spent testing. On T3 Code's GitHub CI, every one of ~15 jobs spends
8 to 10 seconds checking out and 20 to 24 seconds installing dependencies before running
anything, on every push. Then it reruns every check, even when the change was one
README line, and the merge queue retests everything serially.

Pit removes each of those costs:

| Cost on Actions | What Pit does |
| --- | --- |
| Fresh runner, fresh clone, fresh install per job | **Warm workspaces.** Workers keep a git mirror and persistent workspaces. Checkout applies a delta; `node_modules` and `target` stay. |
| Every task reruns on every push | **Action keys.** A task's cache key hashes its command, its input files and its deps. Unchanged tasks are restored from the cache, including their output files. |
| One machine per job, setup repeated per job | **Groups.** A group's tasks share one workspace and run as a DAG, in parallel. Setup happens once. |
| Push and wait to see if it works | **`pit run`** ships your uncommitted changes as an overlay and runs them on a warm worker. Agents get the same through MCP. |
| Merge queues that serialize everything | **Grouped speculative queue.** Changes that touch disjoint tasks land in parallel lanes; overlapping changes are tested speculatively; docs-only changes land without tests. A candidate fails at its first real failure (the rest of its run is stopped), while every other run reports all of its failures. |

## Quick start

```sh
cargo install --path crates/pit     # or grab a release binary
pit up                               # server + worker + dashboard on http://127.0.0.1:7070
cd your-repo && pit init             # writes pit.config.ts from package.json / Cargo.toml / go.mod
pit run                              # runs it, including uncommitted changes
```

`pit import` converts `.github/workflows/*.yml` into a `pit.config.ts` instead.

## Config

`pit.config.ts` is evaluated by the Node (22.6+) or Bun already on the machine, so the
types are real TypeScript and nothing is installed at runtime. `pit.config.json` works too.

```ts
import type { PitConfig } from "@pit-ci/config";

const source = ["**", "!**/*.md", "!docs/**"];

export default {
  groups: {
    ci: {
      os: ["linux", "macos", "windows"], // one copy of the group per OS
      tasks: {
        install: { run: "bun install --frozen-lockfile", inputs: ["package.json", "bun.lock"], cache: "workspace" },
        lint: { run: "bun run lint", deps: ["install"], inputs: source },
        build: { run: "bun run build", deps: ["install"], inputs: source, outputs: ["dist/**"] },
        test: { run: "bun test", deps: ["build"], inputs: source },
        e2e: { run: "npx playwright test", deps: ["build"], image: "mcr.microsoft.com/playwright:v1.55.0" },
      },
    },
  },
} satisfies PitConfig;
```

| Field | Meaning |
| --- | --- |
| `run` | Shell command. On Linux and macOS it runs like GitHub's `shell: bash`: `bash -eo pipefail` where the machine or image has bash, else `sh -e` with pipefail if that shell has it (busybox does; Debian's dash doesn't). So any failing line fails the task, and so does a failing stage of a pipe (`npm test \| tee log`). Put `set +e` or `set +o pipefail` first to opt out. `cmd /C` on Windows. |
| `deps` | Tasks in the same group that must pass first. Dependents of a failed task are skipped. |
| `inputs` | Globs of tracked files the task reads (`!` excludes). Default: everything. Tighter inputs mean more cache hits and smaller merge queue lanes. |
| `outputs` | Files the task produces. Uploaded on success and restored on a cache hit. |
| `reports` | JUnit XML reports the task writes (`reports/*.xml`). A failure names the failed tests and where they failed: "2 of 42 tests failed: cart.applies coupons (90 == 80) at src/cart.test.ts:42; …", from the test case's `file`/`line` or the first `path:line` in its failure text that is a file in the repo. With a GitHub App, every such failure is an annotation on the check, so it shows on the pull request's diff (sent 50 per request, past the 10-per-step cap Actions annotations have). Old reports are deleted before each attempt. |
| `env` | Variables passed through and hashed into the cache key. |
| `runtime` | Commands whose output is part of the cache key (`["node --version"]`), so a toolchain upgrade never reuses old results. Tasks see the repository's tags, so a build that embeds `git describe` lists `"git describe --tags"` here to rebuild when a new tag lands. |
| `env` (zone, locale) | Every task gets `TZ=UTC`, and on Linux `LANG`/`LC_ALL=C.UTF-8`, as GitHub's hosted runners do, so a date or sort-order test gives the same answer on every worker and a cached result holds on all of them. List `TZ`, `LANG` or `LC_ALL` in `env` to get the worker's own value instead (it is then part of the cache key). |
| `secrets` | Secrets the task needs, by name: from the worker's `--secrets-file` (or its environment), masked in logs (plain, base64, double base64), not in the cache key. Never given to fork pull requests; those under `[trusted]` only to trusted runs (see "Secrets on workers"). |
| `cache` | `"remote"` (default, shared by all workers), `"workspace"` (per warm workspace, for installs), or `false`. |
| `backend` | `"host"` (default), `"sandbox"` (Linux PID, IPC, UTS and optional network namespaces), `"docker"`. |
| `image` | Container image; implies `backend: "docker"`. The container's `$HOME` is `/pit-home`, a directory the worker keeps between runs (per repo: one for trusted runs, one for the rest), so `~/.npm`, `~/.cache/pip`, `~/.cargo` and Go's module cache stay warm. Fork and `--fresh` runs get an empty `/tmp` instead. Homes unused for two weeks are removed, and they count toward `--workspace-max-gb`. |
| `network` | `false` cuts network access in `sandbox` and `docker`. |
| `memory` | Memory limit for `docker` tasks, without swap (`"2g"`, `"512m"`): a local run hits the ceiling a small CI runner has. Any task killed by a signal says which, whether the kernel's OOM killer ran meanwhile, and the most memory its processes were seen using (Linux, sampled; not inside containers), instead of "exit 137". |
| `timeout` | Seconds; default 3600, `0` for no limit. The whole process group is killed. Whatever the task leaves running when it ends is stopped too: its background jobs, and on Linux daemons that left its session (`setsid`, `--daemonize`), found by the `PIT_TRACKING_ID` they inherit (GitHub's runner does this at the end of a job, with `RUNNER_TRACKING_ID`). The log says how many. To keep one running between tasks (a Bazel, Gradle or sccache daemon), start it with `PIT_TRACKING_ID=`; a service later tasks need belongs in `services`. |
| `idleTimeout` | Seconds without any output before the task is stopped as hung; default 900, `0` for no limit. |
| `shards` | Split the task into N groups that spread across workers; each gets `PIT_SHARD` and `PIT_SHARD_COUNT` (`vitest --shard=$PIT_SHARD/$PIT_SHARD_COUNT`). On GitHub, besides one check per shard (`pit / test#1` …), the group gets one check under its own name (`pit / test`): pending while any shard runs, passing only when all do. Require that one, so changing `shards` doesn't leave a branch rule waiting for checks that no longer exist. |
| `shardFiles` | Globs of the files to split (`["e2e/**/*.spec.ts"]`). Pit deals them out longest first from recorded durations and gives each shard its list in `PIT_SHARD_FILES`, one per line (`playwright test $PIT_SHARD_FILES`; `xargs -d '\n'` for names with spaces), and always in the file `PIT_SHARD_FILES_LIST` (`.pit/shards/<task>`), since a very long list is left out of the environment. One shard does not get all the slow files. Needs `shards` of 2 or more; globs that match no files fail the run, and a shard dealt nothing passes without running. |
| `retries` | Extra attempts after a failure. A pass on a retry is reported as `flaky:` and is not cached. |
| `quarantine` | `true`: the task still runs and reports, but its failure does not fail the run or eject a queued change. For a known-flaky task while it is fixed; `pit flaky` finds them. |

### Monorepos: one task per package

A config can export a function. Pit calls it with the repo's JavaScript workspace packages
(from `pnpm-workspace.yaml` or `workspaces` in `package.json`) at the commit being run: each
package's `name`, `dir`, `scripts`, workspace `deps`, and `inputs`, which are globs for its
own directory and every workspace package it depends on. A task per package then reruns only
when something it can see changes, like `nx affected` or `turbo --affected`, and the list
never drifts from the repo:

```ts
import type { PitConfig, PitContext } from "@pit-ci/config";

export default ({ packages }: PitContext): PitConfig => ({
  tasks: Object.fromEntries(
    packages
      .filter((p) => p.scripts.includes("typecheck"))
      .map((p) => [`typecheck:${p.dir}`, { run: `pnpm --filter ${p.name} typecheck`, inputs: [...p.inputs, "tsconfig*.json"] }]),
  ),
});
```

Inputs come from declared dependencies. A package that reads another without listing it
in `package.json` needs that directory added by hand, or a change there will be a cache hit.
[`bench/t3code.packages.pit.config.ts`](bench/t3code.packages.pit.config.ts) is a full example.

### Affected-only tools (`nx affected`, `turbo --affected`)

Every task gets `PIT_BASE_SHA`, also exported as `NX_BASE` and `TURBO_SCM_BASE` (with `NX_HEAD` and
`TURBO_SCM_HEAD` set to the commit), so `nx affected -t test` works with no setup. The base is the
one `nrwl/nx-set-shas` would pick: the newest commit of the default branch whose trusted run
passed and that this commit contains (a red commit is skipped). Without one, it's where the commit
left the default branch, or its parent on the default branch. Workspaces have the full history
(they are clones of the worker's mirror), so there is no `fetch-depth: 0` to remember. A task whose
command mentions the base or `affected` has the base in its cache key. A task that reaches
`nx affected` through a script (`pnpm ci`) is not detected: give it `cache: false`. With
uncommitted changes (`pit run`), `NX_HEAD`/`TURBO_SCM_HEAD` are left unset, so the tools diff the
working tree and count those changes.

### Which runs a group is in

`on` limits a group to some kinds of run, the way `if: github.ref == …` does in Actions:

```ts
e2e: { on: ["queue", "main"], tasks: { test: { run: "npm run e2e" } } },   // not on every PR push
deploy: { on: ["main"], tasks: { ship: { run: "./deploy.sh" } } },
```

The kinds are:
- `pr`: pull requests;
- `push`: any branch push;
- `main`: trusted pushes to the default branch and post-merge runs;
- `queue`: merge queue candidates;
- `schedule`;
- `local`: `pit run`, MCP, and any run created through the API.

A rerun or `/retest` counts as the run it repeats.

The default is all. A left-out group is still recorded, passed with its tasks skipped, so its check
reports (`skipped` on a check run, `success` as a commit status) and a required `pit / e2e` never
sits at "Expected". A task named on a local run (`pit run e2e.test`) or in a schedule runs
whatever its group's `on` says. When another run of the same commit really runs a group, the left-out copy
posts no check.

### Shared resources

`lock: "staging-db"` on a group makes groups with that lock run one at a time, across every run
and repo on the server. Use it for integration tests that share a database, or deploys to one
environment. Waiting groups stay queued, with no limit and no cancellations, and say what they
are waiting for ("waiting: lock `staging-db` is held by run_…"). The lock covers the whole
group, so keep it small.

### Services

A database or cache for a group's tests, without one shared instance or a `docker run` in a task:

```ts
api: {
  services: {
    db: { image: "postgres:16-alpine", env: { POSTGRES_PASSWORD: "pit" }, ports: [5432], ready: "pg_isready -U postgres" },
  },
  tasks: { test: { run: "npm test" } },
}
```

Each service starts when the group's first task that is not a cache hit begins (a group the cache
answers starts none), on a network of the group's own, and is removed after the group, including
on cancel and on a hard stop (a worker that crashed cleans up its own when it starts again with the
same data dir). Pit waits for `ready` to succeed in the container, or for the first port to accept
connections, and if the service exits or isn't ready after 90 seconds, the group fails with its
last log lines.

Tasks find a service through `PIT_SERVICE_DB_HOST` and `PIT_SERVICE_DB_PORT`:
- a `docker` task gets the service's name and container port (`db:5432`);
- a host or sandbox task gets `127.0.0.1` and a random published port, so two groups on one
  worker never fight over 5432.

`command` replaces the image's arguments. Each service's image (by ID), command and variables are
part of its group's cache keys. Services need Docker on the worker, and their `env` values are
visible to the worker's `ps`, so keep them to test credentials.

### Schedules

```ts
schedules: {
  // A nightly `pit verify`: no cache, no warm state, so a bug that warm runs hide shows up.
  verify: { cron: "17 3 * * *", fresh: true, noCache: true },
  // 9:00 on weekdays in New York, summer and winter.
  standup: { cron: "0 9 * * 1-5", timezone: "America/New_York", tasks: ["report"] },
},
```

The server reads `schedules` from the config on each repo's default branch and starts a trusted
run of the branch tip when a cron (five fields, UTC unless `timezone` names an IANA zone)
matches. A zoned schedule follows daylight saving the way GitHub's does: a time the clocks skip
fires at the next valid minute, and a time they repeat fires once. It checks twice a minute and
records each slot, so a slot fires once. A slot missed while the server was down (a restart or
an upgrade at 03:00) fires once when it is back, if it fell in the last day, like a systemd timer
with `Persistent=true`. After a longer outage each schedule fires once, never a backlog. A bad
cron is a config error.

### Tool caches

Workspaces stay warm between runs. Files a commit did not change keep their modification
times, and ignored directories survive. So the incremental caches of the tools themselves work
with no extra setup: ESLint's `--cache`, `tsc --build`/`incremental`, and caches under
`node_modules/.cache`. On ephemeral runners these need an `actions/cache` step, and they often
miss anyway, because a fresh checkout gives every file a new modification time. Pit's own
cache does not depend on any of this: it keys on file contents. Check that a tool's cache gives
the same answer as a clean run before relying on it. `knip --cache` did not, on T3 Code
(research P24). The same goes for anything an earlier run left in an ignored directory:
`pit verify` runs in a new, empty workspace (deleted afterwards), the way a brand-new runner
would, so a bug hidden by warm state shows up there. It is cold, so it costs a full install.

### Docker builds

Workers are long-lived, so their Docker daemon keeps its layer cache and BuildKit
`--mount=type=cache` contents between runs; no `cache-from`/`cache-to` setup is needed. Run
`docker build` as a host task with `cache: "workspace"` (or `false`), not the default remote
cache: the image lives in that worker's daemon, not in the task's outputs, so a remote cache
hit on another worker would skip a build whose image is not there.

Images for `docker` tasks are pulled once per worker and then run from its local store, so a
fleet does not hit Docker Hub's pull limits (10 per hour per IP when unauthenticated). The
flip side is that a tag like `node:22` stays at whatever it pointed to when that worker pulled
it. The local image ID is part of the task's cache key, so results never cross image versions
("`docker image node:22` changed (sha256:… → sha256:…)"). Run `docker pull` on the workers to
move them to a new image, or pin a digest (`node:22@sha256:…`).

## How it works

```
            GitHub (source of truth)
   webhooks │ ▲ check runs / statuses / merges
            ▼ │
   ┌──────────────────────────────┐      ┌───────────┐
   │ pit server                   │◄────►│ dashboard │ (embedded)
   │ planner · scheduler · queue  │ SSE  └───────────┘
   │ SQLite · CAS (BLAKE3)        │◄──── pit run / pit mcp (overlays of local changes)
   │ Turborepo + Bazel caches     │
   └──────────────▲───────────────┘
                  │ outbound long-poll (works behind NAT)
     ┌────────────┼─────────────┬──────────────┐
  pit worker   pit worker    pit worker     pit worker
  Linux        Linux         macOS          Windows
  └── git mirror · warm workspaces · node-local CAS · host / sandbox / docker
```

1. **Plan.** A run is a commit plus an optional overlay (changed files, stored in the CAS).
   The server evaluates the config at that commit and creates one group per group and OS.
2. **Claim.** Workers long-poll; a group is handed out the moment it exists. A worker
   claims a whole group and picks the warmest free workspace for that repo.
3. **Prepare.** The mirror fetches only if the commit is missing. The workspace checks out
   the commit, removes untracked files but keeps ignored ones, then writes the overlay.
   Git LFS files are fetched into the mirror once per worker (only objects it lacks) and every
   workspace, fresh ones included, reads them from there, so CI no longer downloads each LFS
   file on every checkout. This needs `git-lfs` on the worker; without it, or with
   `GIT_LFS_SKIP_SMUDGE=1`, LFS files stay pointer files and the group says so.
   Submodules are checked out at the commit the superproject pins, recursively, each from a mirror
   of its own on the worker, fetched once and shared like the main one. Relative URLs (`../lib.git`)
   resolve against the repo's URL, and private ones use the worker's git credentials or SSH keys.
   `.gitmodules` is repository code, so:
   - fork pull requests get no submodules;
   - a hosted repo's submodules must be on its own host, or one listed in `PIT_SUBMODULE_HOSTS`
     (comma separated). Paths on the worker, and URLs with `?`, `#`, `\`, `%` or brackets
     (so no IPv6 literals), are refused.

   Only a local repo (a dev setup) may use path submodules.
4. **Execute.** For each task in the DAG, the worker computes the action key from git blob
   ids (free, from the index) and overlay digests of the matching inputs, plus dependency
   keys. Cache hit: restore outputs, replay the log. Miss: run, stream logs live, upload
   outputs, record the result.
5. **Report.** Events stream to the CLI, dashboard, MCP clients and GitHub.

**Trusted and untrusted runs.** Runs of code that is, or is about to be, on `main` (pushes
to the default branch, or to `main`/`master`/`trunk` when it is not known, merge queue
and post-merge runs into it) are
trusted. PRs, other branches and every `pit run` are not, and neither is any run created
through the API or carrying uncommitted files. Untrusted runs reuse trusted results, but
what they cache is kept apart and never served to a trusted run, and they get their own
warm workspaces. So a branch cannot plant a build result under the key main will compute
(cache poisoning, CVE-2025-36852) or leave a file in a workspace main reuses. Trust is
decided by the server from the job's lease, not claimed by the worker or the client, and a
lease that is no longer live is refused. Set `PIT_WORKER_TOKEN` on the server and its
workers: claiming jobs then needs it, so holders of the API token cannot act as a worker
(`pit up` sets one for its built-in worker). Without it, every API token holder can. This
does not stop a task on the `host` backend from writing elsewhere on the worker: run
untrusted code with `sandbox` or `docker`. The Turborepo and Bazel caches are split the same
way for tasks that use their job's `$PIT_CACHE_TOKEN` (see "Remote cache for existing tools").

Trusted results are shared by every repo on the server (a key holds no repo). On a server with
an API token, only repos the GitHub webhook registered write them. A repo registered through
the API (anyone with the token can) still has trusted runs on its own default branch, but they
write that repo's own trusted results. Its runs read those plus the shared ones; nothing else
reads them, and its trusted runs still never read a branch's. Warm workspaces, container homes
and worker mirrors are per repo, with a hash of the full name, so `acme/web` and `acme_web`
never share one.

Upgrading a shared server to this version (stop the old server first) does three things once:
- Trusted results stored before are demoted to untrusted, so every repo's next trusted run
  rebuilds them.
- Every warm workspace and worker mirror starts cold.
- The old directories stay until the workspace GC removes them (14 days unused, or sooner past
  `--workspace-max-gb`).

## GitHub

Point a GitHub App (or a token) and a webhook at the server:

| Variable | |
| --- | --- |
| `PIT_GITHUB_APP_ID`, `PIT_GITHUB_PRIVATE_KEY` | App mode: check runs with a task table per group. Key is a PEM or a path. |
| `PIT_GITHUB_TOKEN` | Token mode: commit statuses. |
| `PIT_GITHUB_WEBHOOK_SECRET` | Required. Webhooks are verified with HMAC-SHA256. |
| `PIT_PUBLIC_URL` | Link target for checks. |
| `PIT_REPO_<OWNER>_<NAME>` | Optional clone URL override (a local mirror, for example). |

Webhook URL: `<server>/webhooks/github`, events `push`, `pull_request`, `check_run`,
`issue_comment`. Pushes and pull requests start runs; "Re-run" on a check reruns; adding the
`pit:merge` label to a pull request queues it. A `/retest` comment on a pull request by an owner,
member or collaborator reruns the commit Pit last ran for it, with the same restrictions (a fork's
stays a fork's). Passed tasks come back from the cache, so only what failed runs again. It works
with a plain token too, where checks have no Re-run button.

**Missed webhooks.** GitHub doesn't redeliver a webhook that failed because the server was down
or unreachable. So ten seconds after start, and every ten minutes, Pit asks GitHub for the
default branch's head (if committed in the last three days) and each open pull request updated
in the last three days. Any head commit without a run gets the run its webhook would have
started; forks still wait for the label. Repos Pit has never run are left alone, so installing
it doesn't run every open pull request. After start, a head is only run once it has been missing
for two passes, so a webhook that's merely late doesn't produce a second run. A draft marked
ready while the server was down isn't caught up: `/retest` runs it.

**Skipping.** A pushed commit (or a pull request's head commit) whose message has `[skip ci]`,
`[ci skip]`, `[no ci]`, `[skip actions]`, `[actions skip]` or `[skip pit]` gets no run, as on
GitHub: a release bot's version bump doesn't run CI again. As on GitHub, nothing is reported for
it, so a required check waits rather than passes; three words in a commit message don't get a
change past CI. The merge queue, schedules, reruns and `pit run` always run, so a `/retest`
comment runs a skipped pull request commit.

**Drafts.** Draft pull requests run like any other by default. With `drafts: false` in the
config, a draft's pushes get no run and post no checks. GitHub can't merge a draft, so required
checks just wait. Marking it ready for review (`ready_for_review`, which needs the
`pull_request` event, already subscribed) runs its head commit, and converting it back to draft
(`converted_to_draft`) stops the runs still in progress. This replaces the usual
`if: draft == false` job condition, whose skipped check counts as passed and isn't rerun when the
draft becomes ready.

**Forks.** A pull request from a fork never runs on its own: its code, `pit.config.ts`
included, is someone else's. A maintainer who has read it adds the `pit:run` label, which
runs that one commit. A later push needs the label again, so nobody can swap code in after a
review. The run gets a throwaway workspace, no `secrets` and no `env` values, only a minimal
inherited environment (`PATH`, `HOME`, locale, temp dirs), and the `docker` backend whatever
its config says, so it cannot read the worker's files; only workers with Docker take it. Its
`runtime` commands run in the task's container too, offline. Its `pit.config.ts` is evaluated
in a `node:22-alpine` container (`PIT_CONFIG_IMAGE`) with no network, no capabilities and no
mounts but the file itself, because evaluating it on the server would expose the server's files
(its database, its OIDC signing key). That needs Docker with Linux containers on the server; without
it a fork's `pit.config.ts` run fails, and `pit.config.json` still works. It may read cached results but writes none, so nothing it
builds reaches a same-repo pull request, and it records no shard timings. "Re-run" on its check
keeps all of this, and a fork queued with `pit:merge` is queued at the labeled commit: a later
push drops it from the queue instead of testing the new code with the queue's trust.

The same container evaluates more than forks' configs. On a server started with an API token
(`PIT_TOKEN`), a `pit.config.ts` is evaluated on the server only for a trusted run (default
branch, merge queue, schedule) of a repo the GitHub webhook registered. Everything else runs in
the container: pull requests (same-repo ones too), pushes to other branches, GitHub "Re-run"
and `/retest`, `pit run` and other API runs, uploaded overlays, and schedules and queues of
repos registered through the API. On such a server, a GitHub repo's merge queue is entered only
through the `pit:merge` label; `pit queue add` is refused for it. The first webhook for a repo
always registers it anew, so a repo an API caller registered with the same URL earlier stays
theirs, sandboxed (if it took the name, the GitHub repo gets a suffix: check `/api/repos`).
Servers upgraded from before this check should look over `pit queue list` and `/api/repos` for
entries queued through the API on GitHub repos, and repos adopted by a webhook. Any API token holder can send any of those, and a config
evaluated on the server could read the server's files. So a shared server needs Docker on it for
those `.ts` configs; `pit.config.json` needs nothing. A local `pit up` without a token
evaluates on the host as before: its one user already runs the server.

**Secrets on workers.** Every task inherits the worker's environment (minus `PIT_*`), so a
token exported there can be read by any task, declared or not. Put secrets in
`pit worker --secrets-file FILE` (`KEY=VALUE` lines, `PIT_SECRETS_FILE`) instead: those values
reach only tasks that list them in `secrets`. As in a `.env` file, a quoted value may span
lines until its closing quote, which is how an SSH deploy key or a certificate goes in. In
double quotes, `\n`, `\t`, `\r`, `\"` and `\\` are escapes. Each line of a multi-line secret is
also masked on its own, so printing it line by line or indented doesn't get past masking. On the `host` backend a task still runs as the
worker's user and could read the file itself, so run untrusted code in `docker`.

Any branch's config can list any secret, so secrets a branch must never see (deploy keys,
production tokens) go under a `[trusted]` line in that file:

```
NPM_TOKEN=…
[trusted]
PROD_DEPLOY_KEY=…
```

These reach only trusted runs: the default branch, the merge queue and schedules. On a server
with an API token, only those of repos the GitHub webhook registered. The server decides, not
the config. Anything else that asks gets a note in its log instead. GitHub's Re-run and
`/retest` aren't trusted runs, so to redo a deploy, push again or use `pit schedules run`. On a
worker holding these secrets, every other run's code runs only in the `docker` backend, as
forks' do, so it can't read the secrets file or a trusted task's environment on the host. Its
`runtime` commands run there too. Like a fork's, such a run gets a throwaway workspace (its
container can write `.git`, which the worker's own git would run on the next checkout) and runs
its tasks one at a time. Branch runs fail on such a worker if it has no docker, so
keep deploy secrets on separate workers if branches need the host. Don't also export them in
the worker's environment, which every task inherits.
Upgrade workers before adding a `[trusted]` section: an older worker skips the header and hands
those secrets to every task that asks. A server older than this version never allows them.

**Worker versions.** Workers tell the server which version of the job format they understand.
A worker older than the server gets no jobs (`426 Upgrade Required: … upgrade pit on <name>` in
its log, and a warning in the server's) rather than running jobs whose fields it would ignore,
such as which secrets a job may have. Upgrade the server first, then the workers; until a worker
is upgraded, its jobs wait for one that is.

## Cloud credentials (OIDC)

`pit server --oidc` makes the server an OIDC issuer, so a task with
`oidc: "sts.amazonaws.com"` gets a 10-minute token in `PIT_OIDC_TOKEN` to trade for cloud
credentials instead of storing keys. Its keys are at `<PIT_PUBLIC_URL>/.well-known/openid-configuration`
and `/.well-known/jwks.json`, which a cloud must reach over HTTPS. Tokens are refused unless all
of these hold:

- The server was started with `--oidc`, with both `PIT_TOKEN` and `PIT_WORKER_TOKEN` set.
  Without them, anyone who can reach the server could claim a job and get its token. `pit up`
  never issues tokens.
- The repo was registered by a signed GitHub webhook. The subject uses GitHub's `owner/name`, so
  a repo created through the API cannot take a name a policy trusts.
- The run is trusted (P20): a push to the default branch, a post-merge run, a schedule or a
  merge queue candidate into it. Pull requests, other branches, `pit run`, API-created runs and
  forks get nothing.

| Claim | Value |
| --- | --- |
| `sub` | `repo:<owner/name>:ref:refs/heads/<branch>`, or `repo:<owner/name>:merge_queue` |
| `trusted`, `trigger`, `repo`, `ref`, `sha`, `run_id` | From the run as the server recorded it |

Run the `--oidc` server where no task runs. A task on the same host as the same user could read
the signing key in the data dir (`oidc-es256.p8`, mode 0600) or the worker's environment.

```sh
# AWS: the SDKs read the token from a file
echo "$PIT_OIDC_TOKEN" > /tmp/oidc && AWS_ROLE_ARN=arn:aws:iam::123456789012:role/deploy AWS_WEB_IDENTITY_TOKEN_FILE=/tmp/oidc aws sts get-caller-identity
```

## Scaling workers

Workers are plain processes that dial out to the server, so scaling means starting and
stopping `pit worker`. `GET /metrics` (Prometheus text) has what an autoscaler needs: queued
work and how long the oldest has waited, by OS (`pit_queued_groups{os}`,
`pit_oldest_queued_seconds{os}`), plus running groups, online and busy workers, and cache
counters. The queue series disappear when nothing is waiting; use
`sum(pit_queued_groups) or vector(0)` in KEDA or a Prometheus rule.

Stop a worker with SIGTERM. It stops claiming, finishes the groups it is running, and exits.
A second SIGTERM hands those groups back to the server, which requeues them at once, then stops
their tasks and exits, so a scale-down never waits for the 60s heartbeat timeout. Give the
first signal as long as your longest group takes, for example Kubernetes'
`terminationGracePeriodSeconds`.

## Notifications

`pit server --notify-url https://hooks.slack.com/services/…` (or `PIT_NOTIFY_URL`) posts
when a trusted line changes state: main (or the default branch) after a push or a landing, and
each schedule separately. It posts once when it breaks ("Pit: app main is broken:
default.test failed. <link>") and once when it is fixed, and nothing while it stays broken or
green. PR, local and merge queue runs never notify. The body is JSON with `text`, so a Slack
incoming webhook works as is, plus `event` (`broken`/`fixed`), `repo`, `ref`, `trigger`,
`run`, `url` and `failed` for anything else.

## Merge queue

```sh
pit queue add my-branch      # or label a PR `pit:merge`
pit queue list
```

For each change, Pit merges it onto the target tip (`git merge-tree`, no worktree) and
computes its **affected tasks**: tasks whose inputs it touches, plus their dependents.

- **No tasks affected**: lands immediately.
- **Disjoint from every active lane**: opens a new lane, tested on the current tip. Lanes
  land independently. If another lane landed first, the change is re-merged onto the new
  tip; that is safe because the lanes share no affected tasks.
- **Overlaps one lane**: appended to it and tested speculatively on top of the previous
  candidate (`tip+A`, `tip+A+B`). If a candidate fails, that change is ejected and the
  changes after it are retested without it.
- **Overlaps several lanes**: waits until they drain.

**Flakes.** In the queue every failed task is retried once. If it passes on the retry,
the change keeps its place and the task is reported as `flaky:` (and counted in
`/api/stats`), so one flaky test cannot eject an unrelated change and restart everything
behind it. Pull request and local runs stay strict, and a flaky pass is never cached, so
the flake keeps showing until it is fixed.

**Batching.** A candidate contains every earlier change in its lane, so when it passes, the
changes before it pass with it: their own runs are cancelled and they land together. A change
that joins a lane while the previous candidate is still waiting for a worker replaces that run,
and only the newest candidate is tested. If a batched candidate fails, the changes folded into
it get their own runs again to find the one that broke it. Ten overlapping changes on two slots
landed in 12.9s with 3 runs, against 29.9s and 10 runs before.

**Priority.** Workers take merge queue groups first, since every change waiting to land
waits on them, then other trusted runs (main pushes, post-merge), then PRs and local runs,
oldest first within each.

Landing is a non-forced push of the exact commit that was tested. Every landed commit also
gets a full post-merge run as a backstop for undeclared inputs.

## Remote cache for existing tools

The server speaks the protocols your build tools already use, backed by the same CAS:

```sh
TURBO_API=http://127.0.0.1:7070 TURBO_TOKEN=$PIT_TOKEN TURBO_TEAM=me turbo run build
bazel build //... --remote_cache=http://127.0.0.1:7070/bazel
```

Inside a Pit task, use the job's own token rather than `$PIT_TOKEN`:

```sh
TURBO_API=$PIT_CACHE_URL TURBO_TOKEN=$PIT_CACHE_TOKEN TURBO_TEAM=me turbo run build
bazel build //... --remote_cache=$PIT_CACHE_URL/bazel \
  --remote_header=Authorization="Bearer $PIT_CACHE_TOKEN"
```

A request with a job's token gets that run's trust, the same as Pit's own cache:
- a pull request's or branch's builds write artifacts that only untrusted builds reuse;
- main's builds, trusted, write what everyone reuses and never read a branch's.

So a branch can't plant an artifact under a hash main will compute, which is cache poisoning
([CVE-2025-36852](https://nx.dev/blog/cve-2025-36852-critical-cache-poisoning-vulnerability-creep)).
The token works only while its job runs, and only on these cache routes; it can't report
results. Bazel's CAS is shared whatever the trust, because every blob is checked against its
sha256. Requests with the API token keep full access, so don't hand `$PIT_TOKEN` to pull
request builds. On a server without an API token, a build that leaves the job token out gets
full access too, so the split protects main only on a server with one. A task in a container
reaches `$PIT_CACHE_URL` only if that address works from inside the container (not
`127.0.0.1`).

## Agents (MCP)

```json
{ "mcpServers": { "pit": { "command": "pit", "args": ["mcp"] } } }
```

Tools: `pit_run` (run on the working tree, returns a summary and the logs of failures),
`pit_runs`, `pit_logs`, `pit_queue_add`, `pit_queue`. An agent can check its change in
seconds without pushing.

## Commands

| Command | |
| --- | --- |
| `pit up` | Server, worker and dashboard in one process. |
| `pit server` / `pit worker --server URL` | Run them separately, on as many machines as you like. |
| `pit run [task…] [-v] [--json] [--no-cache] [--fresh] [--debug]` | Run on the working tree. Exit code reflects the result. `--fresh` uses a new, empty workspace. `--debug`: a failed task's result names a script on its worker; `sh <script>` opens a shell where it failed (its container, its variables; secrets left out), `sh <script> '<command>'` runs one there. The workspace is kept for 30 minutes or until the script is deleted. Unix workers only. |
| `artifacts` | Globs of files to keep from every run, passed or failed (`["playwright-report/**", "dist/app"]`): screenshots and traces of a failed e2e, a build to try. Each file is served on its own, no zip, at `GET /api/tasks/<id>/artifacts/<path>`, and `pit artifacts <run> [--task t] [--out dir]` downloads them all. Only files the task wrote or changed are kept (a warm workspace's leftovers and committed files the glob also matches are not, and nothing is deleted). Kept up to 14 days with the run's logs, within a quarter of `--cache-max-gb` (newest runs first); at most 100 MiB a file, 1000 files and 512 MiB a task. Unlike `outputs`, not restored on a cache hit. |
| summaries | Every task gets `$PIT_SUMMARY`, also as `$GITHUB_STEP_SUMMARY`: a markdown file to append to, the way Actions job summaries work, so tools that already write one (test loggers, coverage reporters) need no change. It appears on the group's GitHub check under the task's name (up to GitHub's size limit) and at the end of `pit run`. Masked like logs; the last attempt's counts. |
| `pit flaky [--days 14] [--tests]` | Tasks that passed only on a retry, or both passed and failed with the same inputs. `--tests` lists the tests inside tasks instead (from their JUnit `reports`): a test that failed and then passed on the task's retry, or failed where the same inputs passed elsewhere. A pass on a retry also names its flaky tests in the result. |
| `pit slow [--days 14]` | Tasks whose last 5 passing runs take clearly longer (1.3x and 1s more) than the ones before. A single pass at 1.5x and 2s over its usual (median of the last 10) also says `slow: took …, usually …` in its result, on the CLI, the MCP summary and the GitHub check. |
| `pit verify` | `pit run --no-cache --fresh`: proves results depend neither on the cache nor on files earlier runs left in a warm workspace. |
| `pit runs`, `pit logs RUN TASK`, `pit stats` | Inspect. |
| `pit schedules`, `pit schedules run NAME [--repo R]` | List each repo's `schedules` (from its default branch) with when each fires next, in its own zone, and how its last run went; a config that doesn't load is listed as the reason none fire. `run` starts a schedule now, exactly as its cron would (the default branch's tip, as a trusted `schedule:NAME` run), without using up the next slot. `--repo` also fetches that repo first, for a schedule just pushed; without it, the tip as last fetched runs. Anyone with the API token can start any schedule this way, a deploy's included: the code and settings are the default branch's, but the timing is theirs. |
| `pit cancel RUN` | Cancel a queued or running run. Its running tasks are killed (process group, or container) at the worker's next heartbeat, within about 2s. Interrupting `pit run` (Ctrl-C, or SIGTERM from a timeout or an agent) cancels its run the same way and exits 130; interrupt again to quit without waiting. |
| `pit queue add BRANCH`, `pit queue list` | Merge queue. |
| `pit init`, `pit import` | Write a config from the repo or from Actions workflows. |
| `pit mcp` | MCP server over stdio. |

**Cache size.** The server keeps its cache under `--cache-max-gb` (default 20, `0` for
unlimited, env `PIT_CACHE_MAX_GB`). Past the limit it evicts the least recently used task
results and Turborepo/Bazel artifacts, then deletes blobs nothing refers to, until usage is
under 90% of the limit, the way bazel-remote does. Blobs younger than two minutes and logs of
runs from the last 14 days are kept. Evictions are logged, counted in `/api/stats`, and a
task whose last result was evicted says so when it reruns. `POST /api/gc` collects now.

**Run history.** Finished runs are kept for `PIT_HISTORY_DAYS` (default 90, `0` keeps them
forever), with their groups, tasks and test results; their logs only for 14 days anyway. Space
the database frees goes back to the filesystem at each collection. A database from an older
version is rewritten once at startup for that (a few seconds for a large one).

**Workspace disk.** Workers keep a warm workspace per repo and group, `node_modules` and
`target` included. Every ten minutes a worker removes idle workspaces unused for 14 days, and
past `--workspace-max-gb` (default 50, `0` for no size limit, env `PIT_WORKSPACE_MAX_GB`) the
least recently used idle ones, so repos and groups nobody builds any more do not fill the disk.
A removed workspace only costs a cold run. Git mirrors, the worker's blob cache and Docker
images are not trimmed.

Data lives in `~/.pit` (`PIT_DATA`). Set `PIT_TOKEN` on the server and clients to require
a bearer token.

## Status

Working today: everything above, tested end to end (`cargo test`), on Linux, macOS and
Windows workers. Not yet: Firecracker microVM snapshots (the worker reports KVM so the
backend can slot in), VM isolation on macOS and Windows (they use `host`, so run trusted
repos only there), sparse checkouts, and test sharding across workers.

## Development

```sh
cd packages/web && bun install && bun run build   # dashboard, embedded at compile time
cargo test
cargo run -- up
```

Pit tests itself with `pit run` (see `pit.config.ts`).

License: Apache-2.0.
