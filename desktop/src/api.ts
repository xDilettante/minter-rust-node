import { invoke } from "@tauri-apps/api/core";

export interface Settings {
  mode: "light" | "full";
  dataDir: string;
  apiPort: number;
  p2pPort: number;
  storageProfile: "full" | "archive" | "custom";
  keepVersions: number;
  keepEventBlocks: number;
  keepNativeBlocks: number;
  fullStart: "resume" | "checkpoint" | "genesis";
  genesis: string;
  checkpoint: string;
  checkpointSha256: string;
  peers: string[];
  lightAnchor: string;
  primary: string;
  witness: string;
}

export interface NodeStatus {
  network: string;
  height: string;
  appHash: string;
  blockTime: string;
  version: string;
  catchingUp: boolean;
  stale: boolean;
  trustExpiresAt: string | null;
}

export interface Status {
  phase: "stopped" | "starting" | "running" | "syncing" | "attached" | "error";
  managed: boolean;
  controllable: boolean;
  controlId: string | null;
  node: NodeStatus | null;
  error: string | null;
  previousError: string | null;
  freeBytes: number | null;
}

export const api = {
  settings: () => invoke<Settings>("load_settings"),
  save: (settings: Settings) => invoke<void>("save_settings", { settings }),
  start: () => invoke<void>("start_node"),
  stop: (controlId: string | null) => invoke<void>("stop_node", { controlId }),
  status: () => invoke<Status>("node_status"),
  logs: () => invoke<string>("node_logs"),
  choose: (directory: boolean) =>
    invoke<string | null>("choose_path", { directory }),
  exit: (controlId: string | null, acknowledgedError: boolean) =>
    invoke<void>("exit_app", { controlId, acknowledgedError }),
};
