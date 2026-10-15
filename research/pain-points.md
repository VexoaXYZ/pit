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
