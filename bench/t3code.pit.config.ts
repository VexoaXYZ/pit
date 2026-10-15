// Pit config mirroring T3 Code's .github/workflows/ci.yml (lint, typecheck,
// build, test, test_web). Everything runs in one warm workspace: dependencies
// install once and stay installed, and each task reruns only when its inputs change.
import type { PitConfig } from "@pit-ci/config";

const source = [
  "**",
  "!.repos/**",
  "!**/*.md",
  "!docs/**",
  "!.github/**",
  "!apps/mobile/**",
  "!apps/marketing/**",
];

export default {
  groups: {
    ci: {
      os: ["linux"],
      tasks: {
        install: {
          run: "vp install",
          inputs: [
            "package.json",
            "**/package.json",
            "pnpm-lock.yaml",
            "pnpm-workspace.yaml",
            "patches/**",
            "!.repos/**",
          ],
          cache: "workspace",
        },
        electron: {
          run: "vp run --filter @t3tools/desktop ensure:electron",
          deps: ["install"],
          inputs: ["apps/desktop/package.json", "pnpm-lock.yaml"],
          cache: "workspace",
        },
        knip: { run: "vp run knip:check", deps: ["electron"], inputs: source },
        check: { run: "vp check", deps: ["electron"], inputs: source },
        typecheck: { run: "vp run typecheck", deps: ["electron"], inputs: source },
        build: {
          run: "vp run build:desktop && node apps/desktop/scripts/verify-preload-bundle.mjs",
          deps: ["electron"],
          inputs: source,
        },
        "test-web": {
          run: "vp run --filter @t3tools/web test",
          deps: ["install"],
          inputs: [
            "apps/web/**",
            "packages/**",
            "package.json",
            "pnpm-lock.yaml",
            "vite.config.*",
            "tsconfig*.json",
          ],
        },
        test: {
          run: "vp run --parallel --concurrency-limit 4 --filter '!t3' --filter '!@t3tools/monorepo' --filter '!@t3tools/web' test",
          deps: ["electron"],
          inputs: [
            "packages/**",
            "apps/desktop/**",
            "package.json",
            "pnpm-lock.yaml",
            "vite.config.*",
            "tsconfig*.json",
          ],
        },
      },
    },
  },
} satisfies PitConfig;
