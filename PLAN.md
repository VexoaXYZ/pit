# Pit: CI that's never cold

Pit is an open-source CI orchestrator for GitHub repos. GitHub stays the source of truth. Pit replaces Actions and the merge queue with warm workers, content-addressed caching, affected-only task graphs, and grouped merges. It's one Rust binary that you host yourself.

## Decisions (locked)

| Area | Decision |
| --- | --- |
| Wedge | CI orchestrator on top of GitHub. No forge and no new VCS yet |
| First customer | Ourselves and T3-style teams: TS monorepos with heavy agent use |
| Compute | Whoever runs it hosts it. Open source and self-hosted |
| License | Apache-2.0 (or MIT) |
| Language | Rust core, plus a TypeScript dashboard and config SDK |
| Config | `pit.config.ts` (typed tasks with inputs and outputs), plus `pit import` from Actions YAML |
| Merge model | Grouped speculative queue: disjoint PRs merge in parallel, overlapping PRs are batched and bisected |
| Isolation | Tiered. Firecracker microVM with snapshot restore when KVM exists, then container, then host |
| Platforms | Linux, macOS and Windows workers |
| Agent API | Core MVP feature: `pit run` on uncommitted diffs, plus an MCP server |
| Team and timeline | 1–2 people plus agents, about 6 weeks to a usable MVP |

## Targets

Baseline is T3 Code's own CI (`pingdotgg/t3code`), measured on 2026-10-15. It **already runs on Blacksmith 8-vCPU runners**, so we're competing with the fastest commercial runners, not with GitHub's.

| Metric | T3 Code today | Pit target |
| --- | --- | --- |
| `CI` workflow, PR wall-clock | p50 190s, p90 230s, max 352s | **< 20s** for a typical PR |
| `windows-tests` | p50 229s, max 720s | **< 45s** |
| `desktop-macos-preview` | p50 151s | **< 40s** |
| No-op or fully cached re-run | about the same as a full run | **< 2s** |
| Push to first check result | tens of seconds | **< 3s** |
| `pit run` on a local diff, first failure | n/a (push and wait) | **< 5s** |

Stretch benchmarks: `vitejs/vite` PR CI takes about 7.5–9 min, and `vercel/next.js` `build-and-test` takes about 28–85 min.

## Why CI is slow (T3 Code's `ci.yml` as an example)

1. **Cold everything.** Each of the 3 jobs (lint, typecheck, build) boots a fresh runner, checks out, runs `vp` install, ensures Electron, and installs apt packages. That's the same setup 3 times per run, on every push.
2. **Tarball caches.** Dependency caches are restored as archives over the network.
3. **No task cache.** Typecheck reruns on every package even when one file changed.
4. **Job = machine.** Parallelism means more machines, and each one pays the setup cost again.
5. **A serial merge queue.** Every merge invalidates the next PR's test.

## Principles

These are adapted from Anthropic's Scion design, pointed at CI instead of VCS.

1. **Never cold.** Workers are long-lived and keep a warm git mirror, a node-local content-addressed store (CAS), and warm dependency stores. On Linux, Pit restores a Firecracker snapshot of a "ready" VM with deps installed and toolchains loaded, in about 100–300ms.
2. **Checkout means applying a delta.** Push webhooks prefetch into the worker mirror before a job is even scheduled. A workspace is a copy-on-write clone of a warm tree (reflink, overlayfs, APFS `clonefile`, or ReFS block clone) with the diff applied. There's never a `git clone` in the hot path.
3. **Tasks, not jobs.** A task's action key is `hash(inputs + env + toolchain + dep keys)`. On a cache hit, Pit replays logs and outputs instead of running the task. The cache is shared by every worker and by local `pit run`.
4. **Only what changed.** Pit runs only the affected subgraph for a diff. It shards tests by historical duration and runs previously failing and changed tests first.
5. **Groups everywhere.** The scheduler packs related tasks onto one warm worker so they share filesystem state, and fans out only when parallelism actually wins. The merge queue groups PRs by disjoint affected sets.
6. **Stream everything.** Live logs, live check runs, and the first failure surfaced immediately. Agents are first-class clients.

## Architecture

```
            GitHub (source of truth)
   webhooks │ ▲ check runs / merges (GitHub App)
            ▼ │
   ┌─────────────────────────────┐        ┌──────────────┐
   │ pit server (control plane)  │◄──────►│ dashboard    │ TS/React, embedded in binary
   │  webhook ingress · scheduler│  SSE   └──────────────┘
   │  merge queue · graph eval   │
   │  SQLite │ Postgres  (sqlx)  │◄── pit CLI / pit mcp (agents: local diffs)
   │  blobs: disk │ S3/R2/MinIO  │
   └──────────────▲──────────────┘
                  │ outbound-only stream (works behind NAT)
     ┌────────────┼──────────────┬───────────────┐
  pit worker   pit worker     pit worker      pit worker
  Linux+KVM    Linux VPS      macOS           Windows
  firecracker  container      host*           host*
  └── node CAS · warm git mirror (gitoxide) · CoW workspaces · warm dep stores
```
\* Host mode is for trusted repos only. VM isolation on macOS (Virtualization.framework) and Windows (Hyper-V) comes after the MVP.

**One binary, several subcommands:** `pit server`, `pit worker`, `pit run`, `pit mcp`, `pit import`, `pit verify`.

**Crates**

| Crate | Role |
| --- | --- |
| `pit-proto` | Wire protocol (tonic/gRPC or Connect) |
| `pit-server` | Control plane |
| `pit-worker` | Worker daemon |
| `pit-cas` | Content-addressed store, BLAKE3 |
| `pit-git` | gitoxide, with git CLI as a fallback |
| `pit-graph` | Task graph, action keys, affected computation |
| `pit-exec` | Execution backends: firecracker, container, host |
| `pit-queue` | Merge queue |
| `pit-github` | GitHub App integration |
| `pit-cli` | The CLI |

**Packages**

- `packages/config`: the `@pit/config` SDK. The config is evaluated by the repo's own Node or Bun into graph JSON, cached by config hash, so there's no V8 embedded in the binary.
- `packages/web`: the dashboard.

**Data model**

| Entity | Fields |
| --- | --- |
| Repo | — |
| Snapshot | commit + optional overlay diff |
| Run | trigger: `push`, `pr`, `local`, or `queue-batch` |
| TaskExec | action key, status, worker, timings, cache hit |
| Artifact | CAS digest |
| QueueEntry, Lane, Batch | merge queue state |

Every table carries an `org_id` from day one, so a hosted mode isn't blocked later.

## Merge queue: grouped speculative

- For each queued PR, Pit computes `A(pr)`: the affected task set of the PR rebased on current main.
- **Disjoint lanes.** PRs whose `A` sets don't overlap, and that have no textual conflicts, run in independent lanes. Each lane tests only its own affected tasks on current main and merges independently. There's no global lock.
- **Speculative batches.** Overlapping PRs are tested in parallel as `main+A`, `main+A+B`, `main+A+B+C`, with bounded depth. If a batch fails, Pit bisects it. An optional hook hands the failure to an agent to fix.
- **Backstop.** Every merge to main also gets a post-merge full run on main. That catches undeclared inputs, and auto-revert is optional.
- **Fallback mode.** Pit can also act as the runner for GitHub's native merge queue through `merge_group` events.

## Agent-native

- **`pit run [task…]`** diffs against the merge-base, including uncommitted and untracked files. It uploads only the missing blobs to the CAS, runs the affected tasks on a warm worker, and streams the results. It also has `--watch` and `--json` modes.
- **`pit mcp`** exposes `run_affected`, `get_run`, `get_logs`, `rerun_failed` and `queue_status`, for Claude Code, Codex and T3 Code.

## 6-week MVP plan

| Week | Deliverable | Done when |
| --- | --- | --- |
| 0 (2–3 days) | Benchmark harness. Fork `t3code`, then measure a per-phase breakdown of its CI: boot, checkout, install, tasks | Baseline numbers are committed in `bench/` |
| 1 | Cargo workspace and proto. Server on SQLite. Worker registers over an outbound stream, runs a host-mode shell task, and streams logs to the CLI. CAS on disk and S3 | `pit run -- echo hi` round-trips through server and worker |
| 2 | gitoxide mirror with webhook prefetch. CoW workspaces. `@pit/config` produces graph JSON. Action-key hashing, cache hits that skip, log replay | A rerun of an unchanged t3code commit finishes in under 2s |
| 3 | GitHub App and check runs with annotations. Affected-only runs. Dashboard v0. **Shadow mode on the t3code fork**, running alongside Actions and comparing results and times | Pit check results match Actions on 20+ PRs |
| 4 | Firecracker backend with ready-VM snapshot restore. Rootless-container fallback. Grouped task packing. Duration-based test sharding. `pit run` on local diffs, plus `pit mcp` | A typical t3code PR takes under 20s on Linux |
| 5 | Merge queue v1: lanes, speculative batches and bisect. macOS and Windows workers in host mode running t3code's desktop and Windows tests. `pit import` for Actions YAML | t3code's 3-OS matrix is green via Pit, and the queue merges disjoint PRs in parallel |
| 6 | Hardening: secrets policy, fork-PR policy (untrusted code runs only on isolated backends and gets no secrets), retries, flaky-test detection, `pit verify`, install via `curl \| sh` or docker compose, docs | Public repo and launch post with t3code before/after numbers |

**Dogfood hardware:** one bare-metal Linux box with KVM (Hetzner-class), one Mac mini, and one Windows machine. A cheap VPS tests the container fallback.

## Dogfood repos

1. **`pingdotgg/t3code` (fork).** This is the primary. It's a TS monorepo with Electron and a Linux/macOS/Windows matrix, already on Blacksmith, and it's the repo behind Theo's CI complaint.
2. **Pit itself.** Rust and TS, self-hosted on Pit from week 3.
3. **`vitejs/vite` (fork)**, with **`vercel/next.js`** as the launch "holy shit" demo.

## Risks and mitigations

| Risk | Mitigation |
| --- | --- |
| 3 OSes in 6 weeks with 1–2 people | macOS and Windows run in host mode, for trusted repos only, in the MVP. VM isolation comes after |
| Undeclared inputs cause false cache hits | Broad default inputs, the post-merge full run on main, `pit verify` (an uncached diff run), and file-access tracing later |
| Firecracker snapshot gotchas (clock, entropy, network) | Re-seed on restore. Use the container backend when snapshots misbehave |
| GitHub API rate limits | Batch and debounce check-run updates, and use per-installation tokens |
| Running untrusted fork PRs | They run only on microVM or container backends, never get secrets, and workers are outbound-only |
| Actions compatibility becoming a tarpit | `pit import` is a one-way, best-effort translator. We don't run Actions at runtime |

## After the MVP

- VM isolation for macOS and Windows
- Bazel REAPI and Turborepo remote-cache APIs, so Pit becomes the cache backend for existing tools
- A Scion-style fast git layer: server-side diff, blame, merge-base and sparse/virtual checkout APIs
- Flaky-test quarantine, and test impact analysis from coverage data
- An optional hosted offering
