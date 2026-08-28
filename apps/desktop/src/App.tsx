import { useCallback, useEffect, useRef, useState } from "react";
import "./App.css";
import { asBridgeError, engine } from "./engine/client";
import { summarise, type RootSummary } from "./roots";
import { useScan, type Progress } from "./useScan";
import { useRoots } from "./useRoots";
import { Grid, type Sort } from "./Grid";
import { ActionBar } from "./ActionBar";
import { PlanSheet } from "./PlanSheet";
import { Drawer } from "./Drawer";
import { Settings } from "./Settings";
import * as jobs from "./jobs";
import type { BatchView } from "./jobs";
import type {
  Action,
  BatchId,
  CustomCommand,
  Placeholder,
  JobId,
  MenuCommand,
  PlanSheet as Sheet,
  RepoId,
  Selection,
} from "./bindings";

/** The shell: roots on the left, whatever the engine knows on the right. */
export default function App() {
  const [failure, setFailure] = useState<string | null>(null);
  const report = useCallback((e: unknown) => setFailure(asBridgeError(e).message), []);

  const scan = useScan(report);
  const { repos, scanning, progress, errors, lagged } = scan;
  const roots = useRoots(scan, report);

  // Keyed by RepoId, so a row refresh that replaces every row leaves the selection intact.
  const [selected, setSelected] = useState<Set<RepoId>>(new Set());
  const [filter, setFilter] = useState("");
  // The filter text as last committed for evaluation, debounced separately from typing.
  const [query, setQuery] = useState("");
  const [matching, setMatching] = useState<Set<RepoId> | null>(null);
  const [filterError, setFilterError] = useState<string | null>(null);
  const [sort, setSort] = useState<Sort>({ key: "badge", dir: "asc" });
  const filterBox = useRef<HTMLInputElement>(null);
  // Plan and view together, so the confirm button hands back the exact plan shown.
  const [sheet, setSheet] = useState<Sheet | null>(null);
  const [planning, setPlanning] = useState(false);
  // The batch the open sheet would undo, if it is an undo. Routes confirm to
  // `startUndo` instead of `startBatch`.
  const [undoing, setUndoing] = useState<BatchId | null>(null);
  // Everything that has run this session. Folded from events; never persisted.
  const [batches, setBatches] = useState<jobs.Drawer>(jobs.EMPTY);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [placeholders, setPlaceholders] = useState<Placeholder[]>([]);
  useEffect(() => {
    engine.templatePlaceholders().then(setPlaceholders).catch(() => {});
  }, []);
  const dismiss = useCallback(() => {
    setSheet(null);
    setUndoing(null);
  }, []);

  useEffect(() => {
    // `live` closes the window between unmount and the `listen` promise
    // resolving; without it, StrictMode's development remount leaves two
    // listeners briefly, and job states/log lines would apply twice.
    let live = true;
    const pending = engine.onEvents((events) => {
      if (!live) return;
      setBatches((prev) => jobs.apply(prev, events, Date.now()));
      scan.apply(events);
    });
    return () => {
      live = false;
      pending.then((unlisten) => unlisten());
    };
  }, [scan.apply]);

  // Debounce the text only; clearing the box is not typing, so un-filtering is immediate.
  useEffect(() => {
    if (filter.trim() === "") {
      setQuery("");
      return;
    }
    const timer = setTimeout(() => setQuery(filter), 120);
    return () => clearTimeout(timer);
  }, [filter]);

  // The expression is evaluated by the engine; there is no grammar on this side.
  useEffect(() => {
    if (query.trim() === "") {
      setMatching(null);
      setFilterError(null);
      return;
    }
    // Two queries can be in flight across a `repos` change; only the newest may write.
    let live = true;
    engine
      .selectRepos(query)
      .then((ids) => {
        if (!live) return;
        setMatching(new Set(ids));
        setFilterError(null);
      })
      .catch((e) => {
        if (!live) return;
        setMatching(new Set());
        setFilterError(asBridgeError(e).message);
      });
    return () => {
      live = false;
    };
  }, [query, repos]);

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      // Nothing here applies while the sheet is up; Escape closes the sheet itself.
      if (sheetRef.current) return;
      if ((e.metaKey || e.ctrlKey) && e.key === "a") {
        const target = e.target as HTMLElement | null;
        if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) return;
        e.preventDefault();
        setSelected(new Set(shownRef.current.map((r) => r.id)));
      }
      if (e.key === "Escape") setSelected(new Set());
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  useEffect(() => {
    let live = true;
    const pending = engine.onMenu((command) => {
      if (live) menuRef.current(command);
    });
    return () => {
      live = false;
      pending.then((unlisten) => unlisten());
    };
  }, []);

  const hasSelection = selected.size > 0;
  useEffect(() => {
    engine.setHasSelection(hasSelection).catch(() => {});
  }, [hasSelection]);

  /** Plans an action. Never executes: that takes the sheet. */
  async function propose(action: Action, over?: RepoId[]) {
    setFailure(null);
    setPlanning(true);
    const selection: Selection = { type: "Ids", value: over ?? [...selected] };
    try {
      const proposed = await engine.plan(action, selection);
      // Suppress the acknowledgement only for a definition already agreed to;
      // a force-with-lease is never waved through.
      if (
        acknowledged(action) &&
        proposed.view.confirm_guard?.type === "Acknowledge"
      ) {
        proposed.view.confirm_guard = null;
      }
      setSheet(proposed);
      setUndoing(null);
    } catch (e) {
      report(e);
    } finally {
      setPlanning(false);
    }
  }

  async function execute() {
    if (sheet === null) return;
    const { plan, view } = sheet;
    const undoes = undoing;
    setSheet(null);
    setUndoing(null);
    try {
      const batch =
        undoes === null ? await engine.startBatch(plan) : await engine.startUndo(undoes, plan);
      // The drawer has almost certainly already met this batch through its
      // job events, which start before `start_batch` returns.
      setBatches((prev) => jobs.name(prev, batch, view.confirm_label ?? view.headline, plan.action, Date.now()));
      setDrawerOpen(true);
    } catch (e) {
      report(e);
    }
  }

  /** Retries through the sheet, since preconditions may say something different now. */
  function retry(batch: BatchView, repo: RepoId) {
    if (batch.action === null) return;
    void propose(batch.action, [repo]);
  }

  /** Undo proposes through the same sheet as any other action; it never resets directly. */
  async function undoBatch(batch: BatchView) {
    setFailure(null);
    setPlanning(true);
    try {
      setSheet(await engine.planUndo(batch.id));
      setUndoing(batch.id);
    } catch (e) {
      report(e);
    } finally {
      setPlanning(false);
    }
  }

  async function cancelBatch(batch: BatchView) {
    try {
      await engine.cancelBatch(batch.id);
    } catch (e) {
      report(e);
    }
  }

  const reloadTranscript = useCallback(
    (id: JobId) => {
      if (lagged === 0) return;
      engine
        .jobLog(id)
        .then((lines) => setBatches((prev) => jobs.reload(prev, id, lines)))
        .catch(report);
    },
    [lagged, report],
  );

  /** Re-probes without a plan sheet, like `fetch_now`: nothing here changes anything. */
  async function refreshSelected() {
    setFailure(null);
    try {
      await Promise.all([...selected].map((id) => engine.refreshRepo(id)));
    } catch (e) {
      report(e);
    }
  }

  function onMenu(command: MenuCommand) {
    const sortBy = (key: Sort["key"]) => setSort({ key, dir: "asc" });
    switch (command) {
      case "AddRoot":
        return void roots.add();
      case "FocusFilter":
        return filterBox.current?.select();
      case "SelectAll":
        return setSelected(new Set(shownRef.current.map((r) => r.id)));
      case "ClearSelection":
        return setSelected(new Set());
      case "Refresh":
        return void (selected.size > 0 ? refreshSelected() : scan.rescan(roots.paths));
      case "RescanRoots":
        return void scan.rescan(roots.paths);
      case "ToggleDrawer":
        return setDrawerOpen((open) => !open);
      case "Fetch":
        return void propose({ type: "Fetch", value: { prune: false, tags: false } });
      case "PullFfOnly":
        return void propose({ type: "Pull", value: { mode: "FfOnly" } });
      case "PullRebase":
        return void propose({ type: "Pull", value: { mode: "Rebase" } });
      case "PullMerge":
        return void propose({ type: "Pull", value: { mode: "Merge" } });
      case "SortByName":
        return sortBy("name");
      case "SortByPath":
        return sortBy("path");
      case "SortByBranch":
        return sortBy("branch");
      case "SortByState":
        return sortBy("badge");
      case "SortByStatus":
        return sortBy("status");
      case "SortByFetch":
        return sortBy("fetch");
    }
  }

  /** A custom command carries its plan's acknowledgement guard unless already acknowledged. */
  function acknowledged(action: Action): boolean {
    if (action.type !== "Custom") return false;
    const argv = JSON.stringify(action.value.args);
    return roots.config.custom.some(
      (c) => c.acknowledged && JSON.stringify(c.args) === argv,
    );
  }

  async function removeRoot(path: string) {
    setSelected(new Set());
    await roots.remove(path);
  }

  const summaries = summarise(roots.paths, repos, errors);
  const blocked = summaries.filter((s) => s.looksBlocked);
  const shown = matching === null ? repos : repos.filter((r) => matching.has(r.id));
  const shownRef = useRef(shown);
  shownRef.current = shown;
  const sheetRef = useRef(sheet);
  sheetRef.current = sheet;
  const menuRef = useRef(onMenu);
  menuRef.current = onMenu;

  return (
    <div className="app">
      <div className="titlebar" data-tauri-drag-region>
        <span className="titlebar__name" data-tauri-drag-region>
          git-scylla
        </span>
        {scanning && <ScanProgress progress={progress} />}
      </div>
      <div className="body">
        <aside className="sidebar">
          <div className="sidebar__head">
            <span>Roots</span>
            <button onClick={() => void roots.add()}>Add…</button>
            <button onClick={() => setSettingsOpen(true)} aria-label="Settings">
              ⚙
            </button>
          </div>
          {summaries.length === 0 && <p className="sidebar__empty">None yet.</p>}
          <ul className="sidebar__list">
            {summaries.map((root) => (
              <RootRow key={root.path} root={root} onRemove={() => void removeRoot(root.path)} />
            ))}
          </ul>
        </aside>
        <main className="content">
          {failure && <p className="error">{failure}</p>}
          {blocked.length > 0 && <FullDiskAccessHint roots={blocked} onFail={report} />}
          <div className="toolbar">
            <input
              className="filter"
              ref={filterBox}
              placeholder="Filter — behind:>0 &amp; !dirty"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              spellCheck={false}
            />
            {filterError && <span className="filter__error">{filterError}</span>}
            {selected.size > 0 && (
              <button className="inline-action" onClick={() => setSelected(new Set())}>
                Clear
              </button>
            )}
          </div>
          <ActionBar
            count={selected.size}
            busy={planning}
            custom={roots.config.custom}
            placeholders={placeholders}
            onAction={propose}
            onRefresh={refreshSelected}
          />

          {shown.length === 0 && !scanning && blocked.length === 0 && roots.paths.length > 0 && (
            <p className="muted">
              {repos.length === 0 ? "No repositories under these roots." : "Nothing matches."}
            </p>
          )}
          <Grid
            rows={shown}
            roots={roots.paths}
            selected={selected}
            onSelected={setSelected}
            onError={report}
            sort={sort}
            onSort={setSort}
          />
        </main>
      </div>
      <Drawer
        state={batches}
        open={drawerOpen}
        onOpen={setDrawerOpen}
        onCancelBatch={cancelBatch}
        onUndoBatch={undoBatch}
        onRetry={retry}
        onOpenTranscript={reloadTranscript}
      />
      {sheet && (
        <PlanSheet sheet={sheet} roots={roots.paths} onConfirm={execute} onCancel={dismiss} />
      )}
      {settingsOpen && (
        <Settings
          custom={roots.config.custom}
          editor={roots.config.editor}
          terminal={roots.config.terminal}
          onClose={() => setSettingsOpen(false)}
          onSave={(command: CustomCommand) =>
            void engine.putCustom(command).then(roots.adopt).catch(report)
          }
          onRemove={(name) => void engine.removeCustom(name).then(roots.adopt).catch(report)}
          onSetEditor={(editor) =>
            void engine.setEditor(editor).then(roots.adopt).catch(report)
          }
          onSetTerminal={(terminal) =>
            void engine.setTerminal(terminal).then(roots.adopt).catch(report)
          }
        />
      )}
    </div>
  );
}

/** Scan progress in the title bar. Rows stream into the grid throughout; nothing blocks. */
function ScanProgress({ progress }: { progress: Progress | null }) {
  const { found, probed } = progress ?? { found: 0, probed: 0 };
  return (
    <span className="titlebar__status">
      <progress className="scan" value={probed} max={Math.max(found, 1)} />
      Scanning {probed}/{found}
    </span>
  );
}

function RootRow({ root, onRemove }: { root: RootSummary; onRemove: () => void }) {
  return (
    <li className="sidebar__root" title={root.path}>
      <span className="sidebar__rootname">{root.name}</span>
      <span className="sidebar__count">{root.looksBlocked ? "!" : root.count}</span>
      <button className="sidebar__remove" onClick={onRemove} aria-label={`Remove ${root.name}`}>
        ×
      </button>
    </li>
  );
}

/**
 * Shown when a root yields nothing and something under it refused to be
 * read: on macOS, an unsigned build without Full Disk Access.
 */
function FullDiskAccessHint({
  roots,
  onFail,
}: {
  roots: RootSummary[];
  onFail: (e: unknown) => void;
}) {
  return (
    <div className="hint">
      <h2>macOS is blocking the scan</h2>
      <p>
        Nothing was found under{" "}
        {roots.map((r) => (
          <code key={r.path}>{r.path}</code>
        ))}
        , and {roots.reduce((n, r) => n + r.unreadable, 0)} places there could
        not be read. That is almost always Full Disk Access: an unsigned build
        is refused by macOS without it, and the refusal looks exactly like an
        empty folder.
      </p>
      <button
        className="hint__action"
        onClick={() => engine.openFullDiskAccessSettings().catch(onFail)}
      >
        Open Full Disk Access settings
      </button>
      <p className="muted">
        Add git-scylla to the list, then quit and reopen it — macOS only applies
        the change to a fresh launch.
      </p>
    </div>
  );
}
