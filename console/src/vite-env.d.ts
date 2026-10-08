/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Idle sign-out after this many minutes without input (default 15). */
  readonly VITE_IDLE_TIMEOUT_MINUTES?: string;
}
