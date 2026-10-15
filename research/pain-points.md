# CI pain points vs Pit

Each entry is a complaint real teams make about CI, with sources, a reproduction run
against Pit on one machine, and a verdict backed by what was measured. Verdicts:
**SOLVED** (reproduced, Pit handles it, evidence below), **PARTIAL**, **NOT SOLVED**.

## Backlog (not yet tested)

| # | Pain point | Sources |
| --- | --- | --- |
| B5 | `actions/checkout` fails intermittently; release jobs scheduled twice | [HN 46614558](https://news.ycombinator.com/item?id=46614558) |
| B6 | Bills keep climbing; teams move CI to one box for a warm cache | [HN 49612772](https://news.ycombinator.com/item?id=49612772) |
| B7 | GitHub outages stop all CI (4 hours in Feb 2026, 7.5 hours in Aug 2026) | [NERVICO](https://nervico.com/en/blog/github-actions-alternatives-ci-cd/) |

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

**Verdict: SOLVED** (updated after P4). Silent misses: explained. Eviction: Pit now evicts
least recently used results under a size limit (P4), and a task whose last result was
evicted says so ("ran because its earlier result was evicted to keep the cache under its
size limit").

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

## P4. A self-hosted cache grows until the disk is full

**Pain.** Self-hosted Turborepo cache servers do not evict: Garden Snail "does not remove old
artifacts" and suggests S3 object expiration ([garden-snail](https://github.com/pkarolyi/garden-snail));
guides for ducktors/turborepo-remote-cache size the disk by artifacts × builds × retention days
and leave cleanup to storage lifecycle rules ([computingforgeeks](https://computingforgeeks.com/self-hosted-turborepo-remote-cache/),
[turborepo-remote-cache](https://www.npmjs.com/package/turborepo-remote-cache)). bazel-remote is
the counterexample: a max cache size (5 GiB default) enforced by deleting least recently used
files ([bazel-remote](https://github.com/buchgr/bazel-remote)). It is also the other half of P1:
GitHub's 10 GB cap evicts silently.

**Reproduction** (`/tmp/painhunt/p4`): a task whose output is 2 MB of random bytes, keyed on
`src/v`; build 15 versions.

**Before** (feat/pit-mvp): no size option at all.

```
CAS grew by 30 MB over 15 builds; total 37 MB
{'cacheEntries': 55, 'casBlobs': 171, 'casBytes': 37153062}
```

**Fix** (research/pain-hunt): `--cache-max-gb` (default 20, `0` unlimited). Every 60 s, if the
CAS is over the limit, Pit sweeps blobs nothing refers to, then evicts least recently used
cache entries and Turborepo/Bazel artifacts a tenth at a time until usage is under 90% of the
limit. Kept: blobs younger than 2 minutes (an upload about to be referenced looks unreferenced
for a moment) and logs and overlays of runs from the last 14 days. Evictions are logged,
emitted as `cache.evicted`, and counted in `/api/stats`; `POST /api/gc` collects on demand. A
worker whose cache hit loses its outputs to eviction mid-restore runs the task instead of
failing.

**A bug the live run caught.** The first version kept evicting while the only bytes over the
limit were young blobs it could not delete yet, so it evicted every entry, newest included,
and the next pass emptied the cache:

```
right after 15 more 2MB builds (new blobs are inside the 2 min grace):
  casBytes=46.6MB limit=20MB entries=20 evicted=50
after grace + periodic gc:
  casBytes=6.6MB limit=20MB entries=0 evicted=70
```

Fixed: young blobs do not count toward the target, so the collector waits for them instead.
Same experiment after the fix, on the live server with a 20 MB limit:

```
right after 15 builds of 2MB each:          casBytes=36.6MB entries=15 evicted=0
next periodic gc, blobs still inside grace: casBytes=36.6MB entries=15 evicted=0
after the grace period:                     casBytes=16.6MB entries=5  evicted=10
newest build cached?   ◆ artifact   cached
cache gc: evicted 10 entries, deleted 20 blobs, freed 20001830 bytes (36649291 -> 16647461, limit 20000000)
```

Covered by the e2e tests `cache_is_kept_under_its_size_limit` (oldest evicted, newest still a
hit, evicted rerun explained) and `gc_does_not_empty_the_cache_for_blobs_it_cannot_free_yet`
(fails on the first version).

**Verdict: SOLVED**, with a limit: the store can exceed the cap for up to about three minutes
(2 minute grace plus the 60 s check) when builds upload faster than that. Unlike bazel-remote,
there is no hard limit that rejects uploads.

## P5. Docker builds start cold on every run

**Pain.** Hosted runners are discarded after each job, so Buildx "rebuilds the full Dockerfile
from layer zero" unless `cache-from` and `cache-to` point at an external backend
([Blacksmith](https://www.blacksmith.sh/blog/cache-is-king-a-guide-for-docker-layer-caching-in-github-actions),
[RunsOn](https://runs-on.com/github-actions/docker-layer-caching/),
[Ken Muse](https://www.kenmuse.com/blog/implementing-docker-layer-caching-in-github-actions/)).
Even then the gha backend shares the 10 GB Actions budget and gets evicted, multi-stage builds
need `mode=max`, and `RUN --mount=type=cache` contents are not carried to the next runner at all
([devopsboys](https://devopsboys.com/blog/docker-build-slow-no-cache-fix-2026),
[Dash0](https://www.dash0.com/faq/cache-docker-images-github-actions),
[devopskit](https://devopskit.in/blog/docker-build-slow-github-actions-fix/),
[DEPT](https://engineering.deptagency.com/how-to-speed-up-docker-builds-in-github-actions)).
"Why your Docker build takes 11 minutes in CI when it takes 20 seconds locally"
([DEV.to](https://dev.to/libme/why-your-docker-build-takes-11-minutes-in-ci-when-it-takes-20-seconds-locally-3foe)).
Listed in the backlog from [Medium](https://medium.com/@akhilesh-mishra/your-github-actions-runners-are-slow-and-you-are-paying-too-much-for-them-5406577314fe).

**Reproduction** (`/tmp/painhunt/p5`): a Dockerfile with an 8 s dependency layer, a
`--mount=type=cache` step that takes 4 s when its cache is cold, then `COPY src`. A Pit host
task runs `docker build` (BuildKit; `docker-buildx` had to be installed on this box), with
Pit's own task cache off so only Docker's cache is measured. Change `src/` between runs
without committing.

**Result** (no Pit changes needed):

```
== run 1 (cold daemon)
    #8 DONE 8.8s                                  (dependency layer)
    #9 5.648 cache mount: cold
passed in 29.9s
== run 2 (source changed, uncommitted)
    #8 CACHED
    #9 CACHED
passed in 6.0s
```

With the cache-mount step moved after `COPY` so it reruns on every change:

```
== source a   cache mount: warm   WALL 7.09s
== source b   cache mount: warm   WALL 5.15s
== source c   cache mount: warm   WALL 5.81s
```

**Two things found.**
- A test task that pipes `docker build` into `grep | tail` reports success even when the
  build fails (the shell returns the last command's status). That bit this reproduction
  once; it is a config mistake, not a Pit bug, but worth knowing.
- Pit's default remote cache is wrong for `docker build`: the image is not a declared
  output, so a hit on another worker would skip a build whose image is not in that
  worker's daemon. Documented in the README ("Docker builds"): use `cache: "workspace"` or
  `false` for image builds.

**Verdict: SOLVED on one box, PARTIAL for a fleet.** On a single worker, layers and cache
mounts persist with no setup: 29.9 s → 5–7 s per rebuild. With several workers each has its
own Docker cache, and Pit's scheduler does not yet route a group back to the worker that ran
it last, so a fleet only stays warm per worker. Worker affinity in the scheduler is the
follow-up.

## P6. Push and pray: "try fix CI" commits, and no way to test CI config locally

**Pain.** "We've somehow normalized a dev loop that involves `git commit -m "try fix"`, waiting
10 minutes, and repeating" ([HN 46908491](https://news.ycombinator.com/item?id=46908491)); there
is no sensible way to test or even lint the CI config locally
([Lobsters](https://lobste.rs/s/hkqnro/github_actions_is_slowly_killing_your)). The usual answer,
nektos/act, shortens the loop but runs an approximation: different images (up to 18 GB for the
closest one), no GitHub API, partial `actions/cache`, manual secrets, limited macOS and Windows,
so guides still say to confirm on real runners
([Better Stack](https://betterstack.com/community/guides/scaling-docker/act-github-actions-tutorial/),
[Stéphane Robert](https://blog.stephane-robert.info/en/docs/pipeline-cicd/github/act/),
[Earthly](https://earthly.dev/blog/using-github-actions-locally/),
[codewithahsan](https://blog.codewithahsan.dev/run-your-github-actions-locally-with-act-faster-feedback-better-debugging/),
[Jordan Marcelino](https://jordan-marcelino.vercel.app/blog/stop-waiting-on-ci-run-github-actions-locally-with-act)).

**Reproduction** (`/tmp/painhunt/p6`): a repo with a bare origin; tasks `lint` (no
`console.log`) and `test` (`sum(2,2) === 4`). Introduce a bug, fix it in two steps, add a task
to the pipeline config, all uncommitted, then commit, push, and run CI on the pushed commit.

**Result:**

```
== 1. introduce a bug (uncommitted)
    ✗ lint   1.1s exit 1   │ 1:exports.sum = (a, b) => { console.log(a); return a - b; };
    ✗ test   3.0s exit 1   │ sum(2,2) = 0
== 2. fix the logic, forget the console.log
    ✗ lint   1.0s exit 1
    ✓ test   2.1s
== 3. edit the pipeline itself (uncommitted): add a typecheck-ish task
    ✓ syntax 85ms   ✓ lint 1.0s   ✓ test 2.1s  ran because src/sum.js changed
loop: 8.8s, commits made: 0
== 4. commit + push, then the CI run for the pushed commit
push run passed in 206 ms
   lint cached   syntax cached   test cached
```

Three iterations in 8.8 s with no commits, including a change to the pipeline. The run for the
pushed commit was entirely cache hits: `pit run` and CI are the same pipeline on the same
workers with the same action keys, not an emulation, so CI had nothing left to do.

**A gap the reproduction found.** Broken configs failed fast and clearly (trailing comma: 87 ms;
dep on a missing task: 143 ms), but a typo'd field was silently ignored:

```
== unknown field (typo: 'imputs')
passed in 149ms · 1 ran, 0 cached          (inputs silently fell back to every file)
```

Fixed: config structs reject unknown fields.

```
error in 101ms · parsing pit.config.json: unknown field `imputs`, expected one of `run`, `deps`,
`inputs`, `outputs`, `env`, `cache`, `backend`, `image`, `network`, `timeout`, `retries` at line 1 column 39
```

Covered by the unit test `typos_in_the_config_are_errors`.

**Verdict: SOLVED**, with the caveat that `pit run` needs a reachable Pit server and workers
for the target OS; it does not run anything on the laptop itself.
