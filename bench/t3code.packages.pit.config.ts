// T3 Code's CI on Pit, one task per workspace package. Same jobs as
// t3code.pit.config.ts, but Pit hands the config the repo's packages, so
// typecheck runs per package and build and test-web depend only on the
// packages they actually import. A change reruns what it can affect.
import type { PitConfig, PitContext, WorkspacePackage } from "@pit-ci/config";

const source = [
  "**",
  "!.repos/**",
  "!**/*.md",
  "!docs/**",
  "!.github/**",
  "!apps/mobile/**",
  "!apps/marketing/**",
];
// Root files every package's tooling reads.
const shared = ["package.json", "pnpm-lock.yaml", "tsconfig*.json", "vite.config.*"];

export default ({ packages }: PitContext): PitConfig => {
  const byName = new Map(packages.map((p) => [p.name, p]));
  const inputsOf = (...names: string[]) => [
    ...new Set(names.flatMap((n) => byName.get(n)?.inputs ?? [])),
    ...shared,
  ];
  const typecheck = (p: WorkspacePackage) => [
    `typecheck:${p.dir}`,
    {
      run: `vp run --filter ${p.name} typecheck`,
      deps: ["electron"],
      inputs: inputsOf(p.name),
    },
  ];
  return {
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
          ...Object.fromEntries(
            packages
              .filter((p) => p.scripts.includes("typecheck"))
              .filter((p) => !p.dir.startsWith("apps/mobile") && !p.dir.startsWith("apps/marketing"))
              .map(typecheck),
          ),
          build: {
            run: "vp run build:desktop && node apps/desktop/scripts/verify-preload-bundle.mjs",
            deps: ["electron"],
            inputs: inputsOf("@t3tools/desktop", "t3"),
          },
          "test-web": {
            run: "vp run --filter @t3tools/web test",
            deps: ["install"],
            inputs: inputsOf("@t3tools/web"),
          },
        },
      },
    },
  };
};
