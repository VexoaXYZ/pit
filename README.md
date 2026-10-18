# Pit

**CI that is never cold.** Pit is an open-source CI orchestrator for GitHub repos. It replaces
Actions runners and the merge queue with warm workers, content-addressed caching and grouped
execution. GitHub stays the source of truth. One Rust binary, hosted by whoever runs it.

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

The same pipeline takes ~190s (p50) on GitHub Actions with Blacksmith runners. [bench/](bench/README.md)
has the full numbers, including where a single machine loses.

## Why

Most CI time isn't spent testing. Every job starts on a fresh machine, clones, installs, and
reruns every check, even for a one-line README change.

| On GitHub Actions | With Pit |
| --- | --- |
| Fresh runner, clone and install for every job | **Warm workspaces.** Workers keep a git mirror and their checkouts; `node_modules` and `target` stay. |
| Every task reruns on every push | **Action keys.** A task's result is keyed on its command, its input files and its deps. Unchanged tasks come from the cache, outputs included. |
| Setup repeated in every job | **Groups.** A group's tasks share one workspace and run as a DAG, in parallel. |
| Push and wait to find out | **`pit run`** runs your uncommitted changes on a warm worker. Agents get the same over MCP. |
| A merge queue that tests one change at a time | **Grouped speculative queue.** Changes that touch different tasks land in parallel; docs-only changes land without tests. |

## Quick start

```sh
cargo install --path crates/pit   # build the binary
pit up                            # server, worker and dashboard on http://127.0.0.1:7070
cd your-repo
pit init                          # writes pit.config.ts from package.json / Cargo.toml / go.mod
pit run                           # runs it, uncommitted changes included
```

`pit import` converts `.github/workflows/*.yml` into a config instead.

## Config

`pit.config.ts` (or `pit.config.json`) in the repo root:

```ts
import type { PitConfig } from "@pit-ci/config";

const source = ["**", "!**/*.md", "!docs/**"];

export default {
  groups: {
    ci: {
      tasks: {
        install: { run: "bun install --frozen-lockfile", inputs: ["package.json", "bun.lock"], cache: "workspace" },
        lint: { run: "bun run lint", deps: ["install"], inputs: source },
        build: { run: "bun run build", deps: ["install"], inputs: source, outputs: ["dist/**"] },
        test: { run: "bun test", deps: ["build"], inputs: source },
      },
    },
  },
} satisfies PitConfig;
```

The fields you'll use most:

| Field | |
| --- | --- |
| `run` | Shell command (`bash -eo pipefail`, like GitHub's `shell: bash`). |
| `deps` | Tasks in the same group that must pass first. |
| `inputs` | Globs of the files the task reads. Tighter inputs mean more cache hits. |
| `outputs` | Files the task produces, restored on a cache hit. |
| `cache` | `"remote"` (default), `"workspace"` (for installs), or `false`. |
| `image` | Run the task in a container. |
| `shards` | Split a slow task across workers. |
| `os`, `arch` (group) | Run a copy of the group per OS or CPU architecture. |
| `cpus` (group) | Only run it on a worker with at least this many CPUs. |

Groups also take `on` (which kinds of run they're in), `lock` (one at a time across runs),
`services` (a database for the tests) and `schedules`. All of it is in
[docs/reference.md](docs/reference.md).

## Connect GitHub

Run `pit server` where GitHub can reach it, with either a GitHub App (check runs) or a token
(commit statuses):

| Variable | |
| --- | --- |
| `PIT_GITHUB_APP_ID`, `PIT_GITHUB_PRIVATE_KEY` | App mode. |
| `PIT_GITHUB_TOKEN` | Token mode. |
| `PIT_GITHUB_WEBHOOK_SECRET` | Required. |
| `PIT_PUBLIC_URL` | The server's URL, for links on checks. |
| `PIT_TOKEN` | API token clients must send. |
| `PIT_WORKER_TOKEN` | Token workers must send to take jobs. |

Add a webhook to `<server>/webhooks/github` for `push`, `pull_request`, `check_run` and
`issue_comment`. Pushes and pull requests then run, and results show as `pit / <group>` checks.
`/retest` on a pull request reruns only what failed. Pull requests from forks run only once a
maintainer adds the `pit:run` label.

To try Pit on a repo without committing a config, point `PIT_CONFIG_<OWNER>_<NAME>` at a
`pit.config.json` on the server.

## Add workers

Workers dial out to the server, so they work behind NAT. Start one on any machine with the
repo's toolchain:

```sh
PIT_TOKEN=… PIT_WORKER_TOKEN=… pit worker --server https://pit.example.com --slots 2
```

Groups are handed to whichever worker is free, and each worker keeps its own warm workspaces.
`--slots` is how many groups a worker runs at once. Workers keep `PIT_MIN_FREE_GB` (default 5)
free on their disk. `GET /metrics` has queue depth for autoscaling.

## Merge queue

Label a pull request `pit:merge` (or `pit queue add <branch>`). Pit merges it onto the target
branch's tip and works out which tasks it affects:

- Nothing affected: it lands at once.
- Different tasks from what's already queued: it's tested in its own lane, in parallel.
- The same tasks: it's tested on top of the changes ahead of it, and a passing candidate lands
  everything before it too.

A failed task is retried once in the queue, so one flaky test doesn't eject an unrelated change.
Landing pushes the exact commit that was tested.

## Commands

| Command | |
| --- | --- |
| `pit up` | Server, worker and dashboard in one process. |
| `pit server`, `pit worker --server URL` | Run them separately, on as many machines as you like. |
| `pit run [task…]` | Run on the working tree. `--no-cache`, `--fresh` (empty workspace), `--debug` (a shell where it failed). |
| `pit verify` | Run with no cache and no warm state. |
| `pit runs`, `pit logs RUN TASK`, `pit cancel RUN`, `pit artifacts RUN` | Inspect and control runs. |
| `pit flaky`, `pit slow` | Tasks that flake, and tasks that got slower. |
| `pit queue add`, `pit queue list` | The merge queue. |
| `pit schedules` | Scheduled runs. |
| `pit init`, `pit import` | Write a config. |
| `pit mcp` | MCP server for coding agents. |

It also serves Turborepo's and Bazel's remote cache protocols, mints OIDC tokens for cloud
deploys, and posts to Slack when main breaks. See [docs/reference.md](docs/reference.md).

## Status

Tested end to end on Linux, macOS and Windows workers. Not yet: Firecracker microVM isolation,
VM isolation on macOS and Windows (they use the `host` backend, so run only trusted repos there),
and sparse checkouts.

## Development

```sh
cd packages/web && bun install && bun run build   # dashboard, embedded at compile time
cargo test
cargo run -- up
```

Pit tests itself with `pit run` (see `pit.config.ts`). License: Apache-2.0.
