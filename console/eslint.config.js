// ESLint flat config: core recommended + typescript-eslint recommended, plus
// the React hooks rules (v7 includes the compiler-derived checks: refs,
// purity, set-state-in-effect, …) for everything under src/.
import js from "@eslint/js";
import { defineConfig } from "eslint/config";
import reactHooks from "eslint-plugin-react-hooks";
import tseslint from "typescript-eslint";

export default defineConfig([
  { ignores: ["dist/**"] },
  {
    files: ["**/*.{js,ts,tsx}"],
    extends: [js.configs.recommended, tseslint.configs.recommended],
  },
  {
    files: ["src/**/*.{ts,tsx}"],
    extends: [reactHooks.configs.flat.recommended],
  },
]);
