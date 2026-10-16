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
| Merge queues that serialize everything | **Grouped speculative queue.** Changes that touch disjoint tasks land in parallel lanes; overlapping changes are tested speculatively; docs-only changes land without tests. |

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
| `run` | Shell command (`sh -c`, or `cmd /C` on Windows). |
| `deps` | Tasks in the same group that must pass first. Dependents of a failed task are skipped. |
| `inputs` | Globs of tracked files the task reads (`!` excludes). Default: everything. Tighter inputs mean more cache hits and smaller merge queue lanes. |
| `outputs` | Files the task produces. Uploaded on success and restored on a cache hit. |
| `env` | Variables passed through and hashed into the cache key. |
| `cache` | `"remote"` (default, shared by all workers), `"workspace"` (per warm workspace, for installs), or `false`. |
| `backend` | `"host"` (default), `"sandbox"` (Linux PID, IPC, UTS and optional network namespaces), `"docker"`. |
| `image` | Container image; implies `backend: "docker"`. |
| `network` | `false` cuts network access in `sandbox` and `docker`. |
| `timeout` | Seconds. The whole process group is killed. |

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
4. **Execute.** For each task in the DAG, the worker computes the action key from git blob
   ids (free, from the index) and overlay digests of the matching inputs, plus dependency
   keys. Cache hit: restore outputs, replay the log. Miss: run, stream logs live, upload
   outputs, record the result.
5. **Report.** Events stream to the CLI, dashboard, MCP clients and GitHub.

## GitHub

Point a GitHub App (or a token) and a webhook at the server:

| Variable | |
| --- | --- |
| `PIT_GITHUB_APP_ID`, `PIT_GITHUB_PRIVATE_KEY` | App mode: check runs with a task table per group. Key is a PEM or a path. |
| `PIT_GITHUB_TOKEN` | Token mode: commit statuses. |
| `PIT_GITHUB_WEBHOOK_SECRET` | Required. Webhooks are verified with HMAC-SHA256. |
| `PIT_PUBLIC_URL` | Link target for checks. |
| `PIT_REPO_<OWNER>_<NAME>` | Optional clone URL override (a local mirror, for example). |

Webhook URL: `<server>/webhooks/github`, events `push`, `pull_request`, `check_run`.
Pushes and pull requests start runs; "Re-run" on a check reruns; adding the `pit:merge`
label to a pull request queues it.

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

Landing is a non-forced push of the exact commit that was tested. Every landed commit also
gets a full post-merge run as a backstop for undeclared inputs.

## Remote cache for existing tools

The server speaks the protocols your build tools already use, backed by the same CAS:

```sh
TURBO_API=http://127.0.0.1:7070 TURBO_TOKEN=$PIT_TOKEN TURBO_TEAM=me turbo run build
bazel build //... --remote_cache=http://127.0.0.1:7070/bazel
```

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
| `pit run [task…] [-v] [--json] [--no-cache]` | Run on the working tree. Exit code reflects the result. |
| `pit verify` | `pit run --no-cache`: proves results do not depend on the cache. |
| `pit runs`, `pit logs RUN TASK`, `pit stats` | Inspect. |
| `pit queue add BRANCH`, `pit queue list` | Merge queue. |
| `pit init`, `pit import` | Write a config from the repo or from Actions workflows. |
| `pit mcp` | MCP server over stdio. |

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
