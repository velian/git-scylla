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

  // Discovery — rows, scan progress, discovery errors — and the roots that feed
  // it. Both are self-contained enough to live outside this file; what is left
  // here is the window: selection, filtering, and the action flow.
  const scan = useScan(report);
  const { repos, scanning, progress, errors, lagged } = scan;
  const roots = useRoots(scan, report);

  // Keyed by RepoId, so a refresh that replaces every row leaves the selection
  // intact — the user chose repositories, not table positions.
  const [selected, setSelected] = useState<Set<RepoId>>(new Set());
  const [filter, setFilter] = useState("");
  // The filter text as last committed for evaluation. Separate from `filter` so
  // that typing is debounced without new rows being able to postpone the query.
  const [query, setQuery] = useState("");
  const [matching, setMatching] = useState<Set<RepoId> | null>(null);
  const [filterError, setFilterError] = useState<string | null>(null);
  // Worst first, so problems surface at the top. Lifted out of the grid because
  // View ▸ Sort By sets it too.
  const [sort, setSort] = useState<Sort>({ key: "badge", dir: "asc" });
  const filterBox = useRef<HTMLInputElement>(null);
  // The sheet is the only path from an action to a running batch. Holding the
  // whole `PlanSheet` — plan and view together — is what lets the confirm
  // button hand back the exact plan the user was shown.
  const [sheet, setSheet] = useState<Sheet | null>(null);
  const [planning, setPlanning] = useState(false);
  // The batch the open sheet would undo, if it is an undo. What routes the
  // confirmation to `startUndo` rather than `startBatch`, so the new batch is
  // marked and cannot itself be undone.
  const [undoing, setUndoing] = useState<BatchId | null>(null);
  // Everything that has run this session. Folded from events in `jobs.ts`,
  // never persisted.
  const [batches, setBatches] = useState<jobs.Drawer>(jobs.EMPTY);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // The template substitution set, asked for once. Served by the engine rather
  // than restated here: help that repeats a table is help that goes stale.
  const [placeholders, setPlaceholders] = useState<Placeholder[]>([]);
  useEffect(() => {
    engine.templatePlaceholders().then(setPlaceholders).catch(() => {
      // Help text that failed to load is not worth an error banner; the field
      // still works, and the placeholders are in the CLI's help too.
    });
  }, []);
  // Stable, so the sheet's Escape handler is bound once rather than on every
  // render of the grid behind it.
  const dismiss = useCallback(() => {
    setSheet(null);
    setUndoing(null);
  }, []);

  useEffect(() => {
    // `live` closes the window between this effect being torn down and the
    // `listen` promise resolving enough to unsubscribe. Without it a remount —
    // which StrictMode does on every mount in development — leaves two
    // listeners briefly, and the drawer's transcripts are the one piece of
    // state where applying an event twice is visible: job states upsert by id,
    // log lines append.
    let live = true;
    const pending = engine.onEvents((events) => {
      if (!live) return;
      // Each fold takes the whole tick rather than one event at a time, so a
      // batch of forty state changes is one re-render apiece.
      setBatches((prev) => jobs.apply(prev, events, Date.now()));
      scan.apply(events);
    });
    return () => {
      live = false;
      pending.then((unlisten) => unlisten());
    };
  }, [scan.apply]);

  // Debounce the *text*, and only the text: it is the one thing a person types,
  // and a round trip per keystroke is waste. Clearing the box is not typing, so
  // un-filtering is immediate.
  useEffect(() => {
    if (filter.trim() === "") {
      setQuery("");
      return;
    }
    const timer = setTimeout(() => setQuery(filter), 120);
    return () => clearTimeout(timer);
  }, [filter]);

  // The expression is evaluated by the engine — there is no grammar on this
  // side. One parser, in the engine, used by the CLI too.
  //
  // `repos` is a dependency because a repository discovered after the query ran
  // may match it. It must not be able to *defer* the query, though, which is
  // what sharing one debounce with the text did: rows arrive on a 50 ms tick
  // during a scan, so the 120 ms timer was reset before it could ever fire and
  // a filter typed while scanning matched nothing until the scan settled.
  useEffect(() => {
    if (query.trim() === "") {
      setMatching(null);
      setFilterError(null);
      return;
    }
    // Two queries can be in flight across a `repos` change, and nothing
    // guarantees they resolve in order. Only the newest may write.
    let live = true;
    engine
      .selectRepos(query)
      .then((ids) => {
        if (!live) return;
        setMatching(new Set(ids));
        setFilterError(null);
      })
      .catch((e) => {
        // A malformed expression shows nothing and says why, rather than
        // silently matching nothing and looking like an empty working set.
        if (!live) return;
        setMatching(new Set());
        setFilterError(asBridgeError(e).message);
      });
    return () => {
      live = false;
    };
  }, [query, repos]);

  // ⌘A selects everything currently shown.
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      // Nothing here applies while the sheet is up. Escape in particular: the
      // dialog closes itself, and clearing the selection on the way out would
      // silently discard the thing the user was in the middle of confirming.
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

  // The menu bar. Every item routes to the same function the window's own
  // control does — the menu is a second way to reach an action, never a second
  // implementation of one.
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

  // Menu items that need a selection are greyed out when there is none. Sent on
  // the empty↔non-empty transition only, so it is a handful of round trips.
  const hasSelection = selected.size > 0;
  useEffect(() => {
    engine.setHasSelection(hasSelection).catch(() => {
      // A menu that failed to grey out is not worth an error banner.
    });
  }, [hasSelection]);

  // Choosing an action plans it. It never executes: that takes the sheet.
  async function propose(action: Action, over?: RepoId[]) {
    setFailure(null);
    setPlanning(true);
    const selection: Selection = { type: "Ids", value: over ?? [...selected] };
    try {
      const proposed = await engine.plan(action, selection);
      // Suppress the acknowledgement for a definition already agreed to. Only
      // that guard: a force-with-lease is never waved through.
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
      // The drawer has almost certainly met this batch already — its first job
      // events are emitted inside `start_batch`, before it returns — so this
      // names one that exists rather than creating one.
      setBatches((prev) => jobs.name(prev, batch, view.confirm_label ?? view.headline, plan.action, Date.now()));
      setDrawerOpen(true);
    } catch (e) {
      report(e);
    }
  }

  // A retry goes back through the sheet rather than straight to the engine.
  // The repository is in whatever state made it fail, so the preconditions may
  // now say something different — and that is worth seeing before running it
  // again.
  function retry(batch: BatchView, repo: RepoId) {
    if (batch.action === null) return;
    void propose(batch.action, [repo]);
  }

  // Undo proposes; it never resets. Same sheet, same confirmation, same skip
  // reasons — undo is not a special case, and `execute` below is what
  // eventually runs it.
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

  // Transcripts are folded from the event stream, which is complete unless the
  // channel dropped events. It says when it does, so re-read on open after a
  // lag rather than showing a transcript with a hole in it.
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

  // Refreshing re-probes; it changes nothing, so it does not go through a sheet
  // — the same narrow exception `fetch_now` gets.
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
        // Nothing selected means "refresh what I am looking at", which is the
        // whole working set — the roots.
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

  // A custom command that has not been acknowledged carries the plan's guard,
  // and one that has does not. The engine always sends the guard — it has no
  // business knowing what a particular person has read — so suppressing it for
  // an acknowledged definition is the shell's job, and doing it here keeps the
  // CLI, which has no saved definitions, asking every time.
  function acknowledged(action: Action): boolean {
    if (action.type !== "Custom") return false;
    const argv = JSON.stringify(action.value.args);
    return roots.config.custom.some(
      (c) => c.acknowledged && JSON.stringify(c.args) === argv,
    );
  }

  async function removeRoot(path: string) {
    // The rows about to disappear may be selected, and a selection of
    // repositories that no longer exist is not one any action should see.
    setSelected(new Set());
    await roots.remove(path);
  }

  const summaries = summarise(roots.paths, repos, errors);
  const blocked = summaries.filter((s) => s.looksBlocked);
  // The filter narrows what is shown; it never changes what is selected, so a
  // selection made before typing survives clearing the box.
  const shown = matching === null ? repos : repos.filter((r) => matching.has(r.id));
  const shownRef = useRef(shown);
  shownRef.current = shown;
  const sheetRef = useRef(sheet);
  sheetRef.current = sheet;
  // The handler closes over state that changes every render, but the listener
  // is bound once. A ref is the join.
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

/**
 * Scan progress that does not block anything.
 *
 * A determinate bar in the title bar, not a modal or an overlay on the grid:
 * rows stream in as they are probed and stay clickable, sortable and selectable
 * throughout. The count is there because a bar alone cannot say whether it is
 * stuck at 40 of 41 or 40 of 4000.
 */
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
 * The highest-value error message in the application.
 *
 * An unsigned build scanning a protected directory finds nothing and looks
 * identical to an empty working set. Saying so, and offering the one button
 * that fixes it, is the difference between a tool that appears broken and one
 * that tells you what to do.
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
