# CI pain points vs Pit

Each entry is a complaint real teams make about CI, with sources, a reproduction run
against Pit on one machine, and a verdict backed by what was measured. Verdicts:
**SOLVED** (reproduced, Pit handles it, evidence below), **PARTIAL**, **NOT SOLVED**.

## Backlog (not yet tested)

| # | Pain point | Sources |
| --- | --- | --- |
| B1 | Push-and-pray dev loop: `git commit -m "try fix"`, wait 10 minutes, repeat; no way to run CI config locally | [HN 46908491](https://news.ycombinator.com/item?id=46908491), [Lobsters](https://lobste.rs/s/hkqnro/github_actions_is_slowly_killing_your) |
| B4 | Docker layer cache does not persist between runs | [Medium](https://medium.com/@akhilesh-mishra/your-github-actions-runners-are-slow-and-you-are-paying-too-much-for-them-5406577314fe) |
| B5 | `actions/checkout` fails intermittently; release jobs scheduled twice | [HN 46614558](https://news.ycombinator.com/item?id=46614558) |
| B6 | Bills keep climbing; teams move CI to one box for a warm cache | [HN 49612772](https://news.ycombinator.com/item?id=49612772) |
| B7 | GitHub outages stop all CI (4 hours in Feb 2026, 7.5 hours in Aug 2026) | [NERVICO](https://nervico.com/en/blog/github-actions-alternatives-ci-cd/) |
| B8 | Pit's own cache never evicts, so disk grows without bound (found in P1) | this file, P1 |

## P1. Cache misses are silent, and eviction makes them random

**Pain.** GitHub Actions caps each repo's cache at 10 GB and evicts least-recently-used
entries in near real time. Teams report CI that is "intermittently slower with nothing in
the job log to explain it" ([WaveHouse#535](https://github.com/Wave-RF/WaveHouse/issues/535)),
eviction adding ~40 minutes to release builds ([objeck-lang#940](https://github.com/objeck/objeck-lang/issues/940)),
nightly caches that are always evicted before the next night ([photocraft#2688](https://github.com/storytold/photocraft/issues/2688)),
and GitHub staff confirming the thrash ([nodejs/node#61436](https://github.com/nodejs/node/issues/61436)).
"Cache keys are confusing, cache misses are silent, eviction is opaque"
([Ian Duncan](https://www.iankduncan.com/engineering/2026-02-05-github-actions-killing-your-team/)).

**Reproduction** (`/tmp/painhunt/p1`): `build` (inputs `src/**`, `tsconfig.json`, outputs `dist/**`)
and `test` (deps `build`, inputs `src/**`, `test/**`). Run, rerun, then edit `tsconfig.json`
without committing.

**Before** (feat/pit-mvp): keys are computed, never hand-written, so there is no key
confusion, and nothing is evicted, so there are no random misses. But a miss was silent:

```
== run 3: change tsconfig.json (uncommitted)
build passed message= None
test passed message= None
```

`test`'s own inputs did not change at all; it reran because `build`'s did, and nothing said so.

**Fix** (research/pain-hunt): the action key is now built from explainable parts (task
definition and environment, dep keys, input files), stored with each cache entry. On a
miss, the worker diffs against the task's last cached run and says why:

```
== C: uncommitted tsconfig.json change
    ✓ build                    2.0s  ran because tsconfig.json changed
    ✓ test                     1.0s  ran because build changed
== D: add a file and break the test
    ✓ build                    2.1s  ran because src/z.ts added
  ✗ default.test: exited with 1 · ran because task definition or environment changed; src/z.ts added; build changed
```

The reason is also on the task in the API and dashboard. Covered by the unit test
`explains_why_a_task_missed_the_cache` and assertions in the e2e test
`local_runs_cache_restore_outputs_and_ship_overlays`.

**Verdict: PARTIAL.** Silent misses: **SOLVED**. Random eviction: does not happen, but only
because Pit never evicts at all, so its disk grows without bound (B8). A size-capped,
least-recently-used eviction that reports what it evicted is still to do.

## P2. A flaky test ejects innocent changes from the merge queue

**Pain.** Flaky tests are said to burn 15–25% of CI compute on reruns
([DEV.to](https://dev.to/byteframe/github-actions-slow-whats-actually-wasting-your-time-4elh)).
In a merge queue it is worse: a flake fails the queued entry, the PR is ejected even
though it did not touch the failing code, and every PR behind it is rebuilt. "A test that
fails 5% of the time … has a 5% chance of blowing up the entire queue on every cycle"
([tenki](https://tenki.cloud/blog/github-merge-queue-setup)). Reports from Sep–Oct 2026:
5 of 12 failed merge groups in one day were flakes
([awslabs/aidlc-workflows#1712](https://github.com/awslabs/aidlc-workflows/pull/1712)),
four groups ejected in an hour at ~20 minutes each for everyone behind them
([KeepOak/Branch-Agent#1152](https://github.com/KeepOak/Branch-Agent/pull/1152)), and
silent drops while the PR page reads CLEAN ([cleat#3307](https://github.com/cleat-team/cleat/issues/3307)).
See also [Mergify#5158](https://github.com/Mergifyio/mergify/discussions/5158),
[aiur#4193](https://github.com/aiur-team/aiur/issues/4193),
[intent#5249](https://github.com/intent-hq/intent/issues/5249),
[learn-ukrainian#9069](https://github.com/learn-ukrainian/learn-ukrainian.github.io/pull/9069),
[iterion#1410](https://github.com/SocialGouv/iterion/issues/1410).
The fix teams converge on: retry once inside the queue only, flag the flake loudly, keep
PR CI strict.

**Reproduction** (`/tmp/painhunt/p2`): a task that fails the first attempt of every run
("flaked: connection reset") and passes the second; queue a branch with an unrelated change.

**Before** (feat/pit-mvp): the innocent change is ejected and never lands.

```
q_1c0c0ad3cdff   failed   innocent → main   default.flaky,default.unit   tests failed in run run_00a9587a48ff
main has b.txt? 0
```

**Fix** (research/pain-hunt): merge queue runs get one automatic retry per task
(`Job.flake_retries`); any task can opt into more with `retries: N`. A pass on a retry is
reported as `flaky: attempt 1: exit 1, passed on attempt 2`, counted in `/api/stats`
(`tasksFlaky`), shown in `pit run` and the dashboard, and **not cached**.

```
q_34de37c48c68   landed   innocent → main   tested
main has b.txt? 1
  flaky passed flaky: attempt 1: exit 1, passed on attempt 2
log: flaked: connection reset / [pit: attempt 1 failed (exit 1); retrying] / pass on retry
```

The first version cached the flaky pass, so a strict `pit run` on the same tree was served
from the cache and the flake disappeared. Flaky passes are no longer cached; on a second
innocent change:

```
q_1f4f7b203856   landed   innocent2 → main   tested
== strict local run on the landed tree
    ✗ flaky                    7ms exit 1
```

Covered by the e2e test `merge_queue_retries_flakes_once_and_flags_them`.

**Verdict: SOLVED** for ejection. Flaky tests still cost a retry each time they flake;
Pit reports and counts them, but does not quarantine them automatically.

## P3. Long logs freeze the browser

**Pain.** GitHub Actions' log viewer locks up on long builds: "logs from long builds
(>10,000 lines) freeze the browser" ([HN 46908491](https://news.ycombinator.com/item?id=46908491)).

**Reproduction** (`/tmp/painhunt/p3`): a task printing 200,000 lines (3.7 MB). Open its log
on the run page in headless Chromium 1243 at 1280×900, measuring main-thread long tasks
with a `PerformanceObserver` (`/tmp/shots/logperf.mjs`).

**Before** (feat/pit-mvp): the panel rendered the whole log as one text node, and while a
task streamed it rebuilt and re-stripped the whole string on every chunk.

```
{"visibleAfterMs":2769,"longTasks":3,"worstMs":1836,"totalBlockedMs":2238}
```

**Fix** (research/pain-hunt): logs over 2,000 lines render virtualized: fixed 18 px lines,
only the lines in view plus a margin are in the DOM, colour codes are stripped only for
those, and the panel shows the line count. Short logs keep the wrapped view.

```
{"visibleAfterMs":700,"longTasks":2,"worstMs":199,"totalBlockedMs":271}
```

Worst freeze 1,836 ms → 199 ms; time to visible 2.8 s → 0.7 s. The panel opens at the tail
("done", "200,002 lines").

**Run overhead from log volume** (warm runs, same task): 0 lines 258 ms total; 20,000 lines
710 ms; 200,000 lines 901 ms, of which ~0.5 s is draining and uploading the 3.7 MB log after
the process exits. A first run on a new repo took 3.0 s, mostly mirror and workspace setup.

**Verdict: SOLVED** for the browser freeze at 200k lines. Long lines do not wrap in the
virtualized view (they scroll horizontally), and logs over 8 MB are still truncated
(head kept) by the worker.
