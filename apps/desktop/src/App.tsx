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

export default function App() {
  const [failure, setFailure] = useState<string | null>(null);
  const report = useCallback((e: unknown) => setFailure(asBridgeError(e).message), []);

  const scan = useScan(report);
  const { repos, scanning, progress, errors, lagged } = scan;
  const roots = useRoots(scan, report);

  const [selected, setSelected] = useState<Set<RepoId>>(new Set());
  const [filter, setFilter] = useState("");
  const [query, setQuery] = useState("");
  const [matching, setMatching] = useState<Set<RepoId> | null>(null);
  const [filterError, setFilterError] = useState<string | null>(null);
  const [sort, setSort] = useState<Sort>({ key: "badge", dir: "asc" });
  const filterBox = useRef<HTMLInputElement>(null);
  const [sheet, setSheet] = useState<Sheet | null>(null);
  const [planning, setPlanning] = useState(false);
  const [undoing, setUndoing] = useState<BatchId | null>(null);
  const [batches, setBatches] = useState<jobs.Drawer>(jobs.EMPTY);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [placeholders, setPlaceholders] = useState<Placeholder[]>([]);
  useEffect(() => {
    engine.templatePlaceholders().then(setPlaceholders).catch(() => {});
  }, []);
  const dismiss = useCallback(() => {
    setSheet(null);
    setUndoing(null);
  }, []);

  useEffect(() => {
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

  useEffect(() => {
    if (filter.trim() === "") {
      setQuery("");
      return;
    }
    const timer = setTimeout(() => setQuery(filter), 120);
    return () => clearTimeout(timer);
  }, [filter]);

  useEffect(() => {
    if (query.trim() === "") {
      setMatching(null);
      setFilterError(null);
      return;
    }
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

  async function propose(action: Action, over?: RepoId[]) {
    setFailure(null);
    setPlanning(true);
    const selection: Selection = { type: "Ids", value: over ?? [...selected] };
    try {
      const proposed = await engine.plan(action, selection);
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
      setBatches((prev) => jobs.name(prev, batch, view.confirm_label ?? view.headline, plan.action, Date.now()));
      setDrawerOpen(true);
    } catch (e) {
      report(e);
    }
  }

  function retry(batch: BatchView, repo: RepoId) {
    if (batch.action === null) return;
    void propose(batch.action, [repo]);
  }

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
        <button
          className="titlebar__settings"
          onClick={() => setSettingsOpen(true)}
          aria-label="Settings"
        >
          ⚙
        </button>
      </div>
      <div className="body">
        <aside className={`sidebar${sidebarCollapsed ? " sidebar--collapsed" : ""}`}>
          <div className="sidebar__head">
            {!sidebarCollapsed && <span>Roots</span>}
            <button
              className="sidebar__toggle"
              onClick={() => setSidebarCollapsed((c) => !c)}
              aria-label={sidebarCollapsed ? "Expand roots" : "Collapse roots"}
            >
              {sidebarCollapsed ? "›" : "‹"}
            </button>
            {!sidebarCollapsed && <button onClick={() => void roots.add()}>Add…</button>}
          </div>
          {!sidebarCollapsed && (
            <>
              {summaries.length === 0 && <p className="sidebar__empty">None yet.</p>}
              <ul className="sidebar__list">
                {summaries.map((root) => (
                  <RootRow key={root.path} root={root} onRemove={() => void removeRoot(root.path)} />
                ))}
              </ul>
            </>
          )}
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
          fetchIntervalSecs={roots.config.fetch_interval_secs}
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
          onSetFetchInterval={(secs) =>
            void engine.setFetchInterval(secs).then(roots.adopt).catch(report)
          }
        />
      )}
    </div>
  );
}

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
