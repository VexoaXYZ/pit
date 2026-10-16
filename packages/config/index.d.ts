/** Where a task runs. */
export type Backend =
  /** Directly on the worker host. */
  | "host"
  /** Linux namespaces: own PID, IPC and UTS namespaces, optionally no network. */
  | "sandbox"
  /** A container on the worker's Docker daemon (set `image`). */
  | "docker";

export type Os = "linux" | "macos" | "windows" | "any";

export interface TaskDef {
  /** Shell command (`sh -c` on Linux and macOS, `cmd /C` on Windows). */
  run: string;
  /** Tasks in the same group that must pass first. */
  deps?: string[];
  /**
   * Globs of tracked files the task reads, relative to the repo root. Prefix a
   * glob with `!` to exclude. Defaults to every file. The task's cache key is a
   * hash of these files, its command and its deps, so tight inputs mean more
   * cache hits.
   */
  inputs?: string[];
  /** Globs of files the task produces. Restored from the cache on a hit. */
  outputs?: string[];
  /**
   * Commands whose output is part of the cache key, e.g. `["node --version"]`,
   * so a toolchain upgrade on a worker never reuses results built with the old one.
   */
  runtime?: string[];
  /** Environment variables to pass through. Their values are part of the cache key. */
  env?: string[];
  /**
   * Environment variables to pass through as secrets: their values are masked
   * in logs (plain and base64-encoded forms) and are not part of the cache key.
   */
  secrets?: string[];
  /**
   * - `"remote"` (default): results are shared by every worker and `pit run`.
   * - `"workspace"`: remembered per warm workspace. Use for dependency installs.
   * - `false` or `"off"`: always run.
   */
  cache?: "remote" | "workspace" | "off" | boolean;
  backend?: Backend;
  /** Container image for the `docker` backend. Setting it implies `backend: "docker"`. */
  image?: string;
  /** Network access for `sandbox` and `docker`. Defaults to true. */
  network?: boolean;
  /** Timeout in seconds. Defaults to 3600 (an hour); 0 disables it. */
  timeout?: number;
  /**
   * Seconds without any output before the task is treated as hung and
   * stopped. Defaults to 900 (15 minutes); 0 disables it.
   */
  idleTimeout?: number;
  /**
   * Extra attempts after a failure. A task that only passes on a retry is
   * reported as flaky. Merge queue runs always get one automatic retry, so a
   * flake does not eject a change that did not cause it.
   */
  retries?: number;
  /**
   * The task still runs and reports, but its failure does not fail the run
   * or eject a merge queue change. For a known-flaky task while it is fixed
   * (`pit flaky` lists candidates). Tasks that depend on it are skipped when
   * it fails.
   */
  quarantine?: boolean;
  /**
   * Globs of JUnit XML reports the task writes, e.g. `["reports/*.xml"]`
   * (Jest, Vitest, `node --test`, pytest, Go via gotestsum, JUnit,
   * cargo-nextest). A failure then names the failed tests. Old reports are
   * deleted before each attempt.
   */
  reports?: string[];
  /**
   * An OIDC audience, e.g. `"sts.amazonaws.com"`. The task gets a 10-minute
   * token signed by the Pit server in `PIT_OIDC_TOKEN`, to trade for cloud
   * credentials instead of storing keys. Never given to fork pull requests.
   */
  oidc?: string;
  /**
   * Split the task into this many shards, each in its own group so they spread
   * across workers. Each gets `PIT_SHARD` (1-based) and `PIT_SHARD_COUNT`:
   * `vitest --shard=$PIT_SHARD/$PIT_SHARD_COUNT`. The task's deps run in every
   * shard (and come from the cache after the first). No task may depend on a
   * sharded task.
   */
  shards?: number;
}

export interface GroupDef {
  /** One copy of the group runs per OS. Defaults to `["any"]`. */
  os?: Os[];
  backend?: Backend;
  image?: string;
  /**
   * Groups with the same lock run one at a time across all runs and repos,
   * e.g. integration tests sharing a staging database. The rest wait in line;
   * none are cancelled.
   */
  lock?: string;
  tasks: Record<string, TaskDef>;
}

export interface PitConfig {
  /**
   * A group is what a worker claims: its tasks share one warm workspace, so
   * setup like `install` happens once, and run in dependency order with
   * independent tasks in parallel. Groups run in parallel across workers.
   */
  groups?: Record<string, GroupDef>;
  /** Shorthand for a single group named `default`. */
  tasks?: Record<string, TaskDef>;
  /**
   * Runs the server starts by itself, read from the config on the default
   * branch: e.g. a nightly `pit verify`, which is
   * `{ cron: "17 3 * * *", fresh: true, noCache: true }`.
   */
  schedules?: Record<string, Schedule>;
}

export interface Schedule {
  /** Five-field cron expression in UTC: minute hour day-of-month month day-of-week. */
  cron: string;
  /** Tasks to run (`task` or `group.task`). Default: all. */
  tasks?: string[];
  /** Run in new, empty workspaces instead of warm ones. */
  fresh?: boolean;
  /** Ignore cached results. */
  noCache?: boolean;
}

/** A JavaScript workspace package (pnpm, npm, Yarn or Bun workspaces). */
export interface WorkspacePackage {
  /** `name` from its package.json. */
  name: string;
  /** Directory relative to the repo root, e.g. `packages/shared`. */
  dir: string;
  /** Names of its package.json scripts. */
  scripts: string[];
  /** Workspace packages it depends on directly. */
  deps: string[];
  /**
   * Input globs for a task on this package: its own directory and those of
   * every workspace package it depends on, directly or not.
   */
  inputs: string[];
}

/** What a config that exports a function receives. */
export interface PitContext {
  /** The repo's workspace packages at the commit being run. */
  packages: WorkspacePackage[];
}

/**
 * The default export: a config, or a function of the repo that returns one.
 * A function can generate one task per package, so a change reruns only the
 * packages it can affect:
 *
 * ```ts
 * export default ({ packages }: PitContext): PitConfig => ({
 *   tasks: Object.fromEntries(
 *     packages
 *       .filter((p) => p.scripts.includes("typecheck"))
 *       .map((p) => [`typecheck:${p.name}`, { run: `pnpm --filter ${p.name} typecheck`, inputs: p.inputs }]),
 *   ),
 * });
 * ```
 */
export type PitConfigExport = PitConfig | ((ctx: PitContext) => PitConfig | Promise<PitConfig>);

export declare function defineConfig(config: PitConfigExport): PitConfigExport;
