# Benchmark: T3 Code's CI on Pit

Measured on 2026-10-15 against [`pingdotgg/t3code`](https://github.com/pingdotgg/t3code)
at `339179f50a`, using [`t3code.pit.config.ts`](t3code.pit.config.ts). That config mirrors the
`lint` (knip + `vp check`), `typecheck`, `build` and `test_web` jobs of its
`.github/workflows/ci.yml`.

## Baseline: GitHub Actions on Blacksmith runners

T3 Code already runs CI on Blacksmith 8-vCPU runners, the fast commercial option. CI wall-clock over the
last 40 successful runs: **p50 190s, p90 230s**.

Per-job breakdown from run 38701079905:

| Job | Total | Checkout | Setup Vite+ (install) | The actual work |
| --- | --- | --- | --- | --- |
| Lint | 76s | 10s | 22s | 41s |
| Typecheck | 48s | 8s | 20s | 17s |
| Build | 70s | 8s | 21s | 36s |
| Test Web | 92s | 9s | 22s | 58s |

All ~15 jobs in the workflow pay the ~30s of checkout and install again on every push,
whether the change was code or a README line.

## Pit, on one shared 8-core VPS

The machine also runs other workloads (load average 11 to 15 throughout), and tasks run
as root.

| Scenario | Pit | What happened |
| --- | --- | --- |
| Cold: nothing cached, repo never seen | 941s | Server and worker each clone from GitHub (~3 min), install is cold (88s), then all 7 tasks run at once on 8 cores. |
| Warm, new commit: dependencies unchanged, code changed | 519s | Workspace ready in **3.2s**; `install` and `electron` **skipped**. All CI tasks rerun, 4 at a time on one box. |
| Warm, nothing relevant changed | **1.0s** | All 7 tasks cached. Workspace ready in 348ms. |
| Warm, uncommitted README edit | **1.1s** | Outside every task's inputs: all cached. |
| Warm, uncommitted edit in `apps/marketing` | **1.0s** | Excluded from these tasks' inputs: all cached. |

### Re-measured on an idle box, with per-package tasks (research P21)

The same box later, with nothing else running. The one-file edits are uncommitted (`pit run`).
[`t3code.packages.pit.config.ts`](t3code.packages.pit.config.ts) generates one typecheck per
package from the repo's workspace, and gives `build` and `test-web` only the packages they import.

| Scenario | Monolithic config | Per-package config |
| --- | --- | --- |
| Cold | 716s | |
| One-file edit in `apps/web` | 443s | 438s (`typecheck` 137s of work → 43s; `test-web` sets the time) |
| One-file edit in `packages/ssh` (`web` does not import it) | reruns everything | **209s** (`test-web` cached, 3 of 30 typechecks) |

Blacksmith's p50 is 190s for any change, because every job reruns every time.

### Reading it honestly

- When nothing a task reads has changed, Pit answers in about a second, against about 190s on
  GitHub. That covers docs, other packages, rebases, re-runs, and merge-queue retests of the
  same tree.
- On a real code change, this single, loaded box is **slower** than GitHub, which spreads the same jobs over
  ~15 dedicated runners (~100 vCPUs). Pit removes the setup (30s per job) but cannot
  invent cores. Add workers (`pit worker` on more machines) and split the config into
  more groups; groups run in parallel across workers.
- Two suites did not pass on this box for environmental reasons, not because of Pit:
  `test` has a filesystem-permission test that cannot fail as root, and `test_web`'s
  120s hook timed out while 4 suites shared 8 cores. Run alone, `test_web` passed
  (331s). The headline numbers use the 7 tasks that pass here.

### Run it yourself

```sh
pit up &
git clone https://github.com/pingdotgg/t3code && cd t3code
cp /path/to/pit/bench/t3code.pit.config.ts pit.config.ts
# knip flags unknown root files; register the config like .coderabbit.config.ts
pit run install electron check knip typecheck build test-web
pit run install electron check knip typecheck build test-web   # warm
```
