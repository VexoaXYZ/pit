// Pit tests itself with Pit. Run `pit run` from the repo root.
import type { PitConfig } from "@pit-ci/config";

const rust = ["crates/**", "Cargo.toml", "Cargo.lock"];
const web = ["packages/web/**", "!packages/web/dist/**"];

export default {
  groups: {
    rust: {
      os: ["linux"],
      tasks: {
        fmt: { run: "cargo fmt --all --check", inputs: rust },
        clippy: { run: "cargo clippy --all-targets --locked -- -D warnings", inputs: rust },
        test: { run: "cargo test --locked", inputs: rust },
      },
    },
    web: {
      os: ["linux"],
      tasks: {
        install: { run: "cd packages/web && bun install --frozen-lockfile", inputs: ["packages/web/package.json", "packages/web/bun.lock"], cache: "workspace" },
        typecheck: { run: "cd packages/web && bun run typecheck", deps: ["install"], inputs: web },
        build: { run: "cd packages/web && bun run build", deps: ["install"], inputs: web, outputs: ["packages/web/dist/**"] },
      },
    },
  },
} satisfies PitConfig;
