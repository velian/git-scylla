/**
 * The typed client for the Rust side.
 *
 * Every signature here is built from `../bindings`, which is generated from the
 * Rust types by `scripts/bindings.sh` and checked for drift in CI. Nothing in
 * this file restates a shape — if a field moves, this stops compiling.
 */
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

/** The channel `events.rs` emits on. */
const CHANNEL = "engine://events";

/** The channel `menu.rs` emits on. */
const MENU = "menu://command";

/**
 * A failed command.
 *
 * Tauri rejects with whatever the command returned, which is a `BridgeError`
 * — `{ kind, message }`, never a stringified Rust error. This narrows it back
 * to that shape so callers can branch on `kind`.
 */
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

  /** Every repository the engine knows, as grid rows. */
  getSnapshot: (): Promise<RepoRow[]> => invoke("get_snapshot"),

  /**
   * The repositories matching a selection expression.
   *
   * Parsed and evaluated in the engine, by the same parser the CLI's
   * `--select` uses. There is deliberately no grammar on this side.
   */
  selectRepos: (expr: string): Promise<RepoId[]> => invoke("select_repos", { expr }),

  refreshRepo: (id: RepoId): Promise<void> => invoke("refresh_repo", { id }),

  /**
   * What an action would do, and the strings that describe it.
   *
   * Both halves come from the engine: `view` is what the sheet displays, `plan`
   * is what it hands back to `startBatch` unmodified. Nothing here composes a
   * phrase: the CLI shows this same view as text, so the two cannot disagree.
   */
  plan: (action: Action, selection: Selection): Promise<PlanSheet> =>
    invoke("plan", { action, selection }),

  startBatch: (plan: Plan): Promise<BatchId> => invoke("start_batch", { plan }),

  cancelBatch: (id: BatchId): Promise<void> => invoke("cancel_batch", { id }),

  /**
   * What undoing a finished batch would do.
   *
   * The same shape a proposed action returns, and it goes through the same
   * sheet: undo is not a special case and does not bypass confirmation. A batch
   * that cannot be undone comes back as an empty plan, which renders as
   * "nothing to do" with no control.
   */
  planUndo: (id: BatchId): Promise<PlanSheet> => invoke("plan_undo", { id }),

  startUndo: (id: BatchId, plan: Plan): Promise<BatchId> => invoke("start_undo", { id, plan }),

  jobLog: (id: JobId): Promise<LogLine[]> => invoke("job_log", { id }),

  /** `null` when the user dismissed the picker, which is a choice and not an error. */
  pickRootDir: (): Promise<string | null> => invoke("pick_root_dir"),

  getConfig: (): Promise<Config> => invoke("get_config"),

  /** Returns the configuration as it now stands — the merge rules may have
   * dropped a nested path or replaced narrower ones. */
  addRoot: (path: string): Promise<Config> => invoke("add_root", { path }),

  removeRoot: (path: string): Promise<Config> => invoke("remove_root", { path }),

  openFullDiskAccessSettings: (): Promise<void> =>
    invoke("open_full_disk_access_settings"),

  setEditor: (editor: string | null): Promise<Config> => invoke("set_editor", { editor }),

  /** `null` means "resolve it", which for a terminal is a working setting. */
  setTerminal: (terminal: string | null): Promise<Config> =>
    invoke("set_terminal", { terminal }),

  /**
   * What `Handoff::Terminal` would use right now.
   *
   * A handoff has no plan sheet, so this is the only place the automatic choice
   * can be seen before it is made — which is what keeps it from being an
   * invisible guess.
   */
  resolvedTerminal: (): Promise<string> => invoke("resolved_terminal"),

  /**
   * The template substitution set, from `core::template`.
   *
   * Asked for rather than restated: help text that repeats a table is help text
   * that goes stale, and adding a placeholder in Rust should not need a second
   * edit here to be documented.
   */
  templatePlaceholders: (): Promise<Placeholder[]> => invoke("template_placeholders"),

  /** Save or replace a custom command. */
  putCustom: (command: CustomCommand): Promise<Config> => invoke("put_custom", { command }),

  removeCustom: (name: string): Promise<Config> => invoke("remove_custom", { name }),

  /**
   * Record that the user has read what a custom command does not get.
   *
   * Separate from `putCustom` because it is a different act, and keeping them
   * apart is what makes "editing the argv clears the acknowledgement"
   * enforceable rather than a convention.
   */
  acknowledgeCustom: (name: string): Promise<Config> => invoke("acknowledge_custom", { name }),

  /** Open one repository elsewhere. Not a job: no transcript, no undo. */
  handOff: (what: Handoff, path: string): Promise<void> =>
    invoke("hand_off", { what, path }),

  /** Fetch one repository without a plan sheet; only Fetch may. */
  fetchNow: (id: RepoId): Promise<BatchId> => invoke("fetch_now", { id }),

  /** Grey out the menu items that need a selection, or restore them. */
  setHasSelection: (has: boolean): Promise<void> => invoke("set_has_selection", { has }),

  /**
   * Subscribe to menu selections.
   *
   * The menu never reaches the engine: an item asks the window to do something
   * the window already does, so there is one implementation of each action
   * rather than one per route.
   */
  onMenu: (handler: (command: MenuCommand) => void): Promise<UnlistenFn> =>
    listen<MenuCommand>(MENU, (e) => handler(e.payload)),

  /**
   * Subscribe to engine events.
   *
   * The Rust side batches on a 50 ms tick, so each delivery is an array of
   * whatever happened in that window rather than one event.
   */
  onEvents: (handler: (events: UiEvent[]) => void): Promise<UnlistenFn> =>
    listen<UiEvent[]>(CHANNEL, (e) => handler(e.payload)),
};
