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

/** Narrows a failed `invoke` to the `BridgeError` shape Tauri rejects with. */
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

  /** Repositories matching a selection expression, parsed and evaluated in the engine. */
  selectRepos: (expr: string): Promise<RepoId[]> => invoke("select_repos", { expr }),

  refreshRepo: (id: RepoId): Promise<void> => invoke("refresh_repo", { id }),

  /** `view` renders the sheet; `plan` is handed back to `startBatch` unmodified. */
  plan: (action: Action, selection: Selection): Promise<PlanSheet> =>
    invoke("plan", { action, selection }),

  startBatch: (plan: Plan): Promise<BatchId> => invoke("start_batch", { plan }),

  cancelBatch: (id: BatchId): Promise<void> => invoke("cancel_batch", { id }),

  /** What undoing a finished batch would do. Returns through the same `PlanSheet` shape as any other action. */
  planUndo: (id: BatchId): Promise<PlanSheet> => invoke("plan_undo", { id }),

  startUndo: (id: BatchId, plan: Plan): Promise<BatchId> => invoke("start_undo", { id, plan }),

  jobLog: (id: JobId): Promise<LogLine[]> => invoke("job_log", { id }),

  /** `null` when the user dismissed the picker. */
  pickRootDir: (): Promise<string | null> => invoke("pick_root_dir"),

  getConfig: (): Promise<Config> => invoke("get_config"),

  addRoot: (path: string): Promise<Config> => invoke("add_root", { path }),

  removeRoot: (path: string): Promise<Config> => invoke("remove_root", { path }),

  openFullDiskAccessSettings: (): Promise<void> =>
    invoke("open_full_disk_access_settings"),

  setEditor: (editor: string | null): Promise<Config> => invoke("set_editor", { editor }),

  /** `null` means resolve it automatically. */
  setTerminal: (terminal: string | null): Promise<Config> =>
    invoke("set_terminal", { terminal }),

  /** What `Handoff::Terminal` would use right now. */
  resolvedTerminal: (): Promise<string> => invoke("resolved_terminal"),

  templatePlaceholders: (): Promise<Placeholder[]> => invoke("template_placeholders"),

  putCustom: (command: CustomCommand): Promise<Config> => invoke("put_custom", { command }),

  removeCustom: (name: string): Promise<Config> => invoke("remove_custom", { name }),

  acknowledgeCustom: (name: string): Promise<Config> => invoke("acknowledge_custom", { name }),

  /** Opens one repository elsewhere. Not a job: no transcript, no undo. */
  handOff: (what: Handoff, path: string): Promise<void> =>
    invoke("hand_off", { what, path }),

  /** Fetch one repository without a plan sheet. Only `Fetch` may. */
  fetchNow: (id: RepoId): Promise<BatchId> => invoke("fetch_now", { id }),

  setHasSelection: (has: boolean): Promise<void> => invoke("set_has_selection", { has }),

  onMenu: (handler: (command: MenuCommand) => void): Promise<UnlistenFn> =>
    listen<MenuCommand>(MENU, (e) => handler(e.payload)),

  /** The Rust side batches on a 50ms tick; each delivery is an array of events. */
  onEvents: (handler: (events: UiEvent[]) => void): Promise<UnlistenFn> =>
    listen<UiEvent[]>(CHANNEL, (e) => handler(e.payload)),
};
