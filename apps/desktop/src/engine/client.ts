/** Typed client for the Rust side. Every function wraps one Tauri `invoke` or `listen` call. */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  Action,
  BatchId,
  BridgeError,
  Config,
  CustomCommand,
  Handoff,
  JobId,
  LogLine,
  MenuCommand,
  Placeholder,
  Plan,
  PlanSheet,
  RepoId,
  RepoRow,
  ScanId,
  Selection,
  UiEvent,
} from "../bindings";

const CHANNEL = "engine://events";
const MENU = "menu://command";

export function asBridgeError(e: unknown): BridgeError {
  if (typeof e === "object" && e !== null && "kind" in e && "message" in e) {
    return e as BridgeError;
  }
  return { kind: "EngineStopped", message: String(e) };
}

export const engine = {
  startScan: (roots: string[], nested = false): Promise<ScanId> =>
    invoke("start_scan", { roots, nested }),

  cancelScan: (id: ScanId): Promise<void> => invoke("cancel_scan", { id }),

  getSnapshot: (): Promise<RepoRow[]> => invoke("get_snapshot"),

  selectRepos: (expr: string): Promise<RepoId[]> => invoke("select_repos", { expr }),

  refreshRepo: (id: RepoId): Promise<void> => invoke("refresh_repo", { id }),

  plan: (action: Action, selection: Selection): Promise<PlanSheet> =>
    invoke("plan", { action, selection }),

  startBatch: (plan: Plan): Promise<BatchId> => invoke("start_batch", { plan }),

  cancelBatch: (id: BatchId): Promise<void> => invoke("cancel_batch", { id }),

  planUndo: (id: BatchId): Promise<PlanSheet> => invoke("plan_undo", { id }),

  startUndo: (id: BatchId, plan: Plan): Promise<BatchId> => invoke("start_undo", { id, plan }),

  jobLog: (id: JobId): Promise<LogLine[]> => invoke("job_log", { id }),

  pickRootDir: (): Promise<string | null> => invoke("pick_root_dir"),

  getConfig: (): Promise<Config> => invoke("get_config"),

  addRoot: (path: string): Promise<Config> => invoke("add_root", { path }),

  removeRoot: (path: string): Promise<Config> => invoke("remove_root", { path }),

  openFullDiskAccessSettings: (): Promise<void> =>
    invoke("open_full_disk_access_settings"),

  setEditor: (editor: string | null): Promise<Config> => invoke("set_editor", { editor }),

  setTerminal: (terminal: string | null): Promise<Config> =>
    invoke("set_terminal", { terminal }),

  resolvedTerminal: (): Promise<string> => invoke("resolved_terminal"),

  templatePlaceholders: (): Promise<Placeholder[]> => invoke("template_placeholders"),

  putCustom: (command: CustomCommand): Promise<Config> => invoke("put_custom", { command }),

  removeCustom: (name: string): Promise<Config> => invoke("remove_custom", { name }),

  acknowledgeCustom: (name: string): Promise<Config> => invoke("acknowledge_custom", { name }),

  setFetchInterval: (secs: number | null): Promise<Config> =>
    invoke("set_fetch_interval", { secs }),

  handOff: (what: Handoff, path: string): Promise<void> =>
    invoke("hand_off", { what, path }),

  fetchNow: (id: RepoId): Promise<BatchId> => invoke("fetch_now", { id }),

  setHasSelection: (has: boolean): Promise<void> => invoke("set_has_selection", { has }),

  onMenu: (handler: (command: MenuCommand) => void): Promise<UnlistenFn> =>
    listen<MenuCommand>(MENU, (e) => handler(e.payload)),

  onEvents: (handler: (events: UiEvent[]) => void): Promise<UnlistenFn> =>
    listen<UiEvent[]>(CHANNEL, (e) => handler(e.payload)),
};
