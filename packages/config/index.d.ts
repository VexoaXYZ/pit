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
  /** Timeout in seconds. */
  timeout?: number;
  /**
   * Extra attempts after a failure. A task that only passes on a retry is
   * reported as flaky. Merge queue runs always get one automatic retry, so a
   * flake does not eject a change that did not cause it.
   */
  retries?: number;
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
}

export declare function defineConfig(config: PitConfig): PitConfig;
