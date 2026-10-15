# CI pain points vs Pit

Each entry is a complaint real teams make about CI, with sources, a reproduction run
against Pit on one machine, and a verdict backed by what was measured. Verdicts:
**SOLVED** (reproduced, Pit handles it, evidence below), **PARTIAL**, **NOT SOLVED**.

## Backlog (not yet tested)

| # | Pain point | Sources |
| --- | --- | --- |
| B1 | Push-and-pray dev loop: `git commit -m "try fix"`, wait 10 minutes, repeat; no way to run CI config locally | [HN 46908491](https://news.ycombinator.com/item?id=46908491), [Lobsters](https://lobste.rs/s/hkqnro/github_actions_is_slowly_killing_your) |
| B2 | Flaky tests burn 15–25% of CI compute on reruns | [DEV.to](https://dev.to/byteframe/github-actions-slow-whats-actually-wasting-your-time-4elh) |
| B3 | Logs of long builds (>10,000 lines) freeze the browser | [HN 46908491](https://news.ycombinator.com/item?id=46908491) |
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
