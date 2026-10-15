// pit.config.ts only needs the types (`import type`), which are erased before
// the file runs. This helper exists for configs that prefer a function call.
export const defineConfig = (config) => config;
