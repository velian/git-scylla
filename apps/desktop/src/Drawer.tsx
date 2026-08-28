/** The job drawer: batches as a clickable list, and the raw transcript behind each job. */
import { useEffect, useState } from "react";
import { basename } from "./columns";
import { elapsed, isRunning, progress, stateLabel, visible } from "./jobs";
import type { BatchView, Drawer as State, JobRow } from "./jobs";
import type { JobId, LogLine, RepoId } from "./bindings";

export function Drawer({
  state,
  open,
  onOpen,
  onCancelBatch,
  onUndoBatch,
  onRetry,
  onOpenTranscript,
}: {
  state: State;
  open: boolean;
  onOpen: (open: boolean) => void;
  onCancelBatch: (batch: BatchView) => void;
  onUndoBatch: (batch: BatchView) => void;
  onRetry: (batch: BatchView, repo: RepoId) => void;
  onOpenTranscript: (id: JobId) => void;
}) {
  const [showBackground, setShowBackground] = useState(false);
  const [selected, setSelected] = useState<JobId | null>(null);
  const [, tick] = useState(0);
  const running = state.batches.some(isRunning);
  useEffect(() => {
    if (!running) return;
    const timer = setInterval(() => tick((n) => n + 1), 1000);
    return () => clearInterval(timer);
  }, [running]);

  const shown = state.batches.filter((b) => visible(b, showBackground));
  const hidden = state.batches.length - shown.length;

  function select(id: JobId) {
    const next = selected === id ? null : id;
    setSelected(next);
    if (next !== null) onOpenTranscript(next);
  }

  return (
    <section className={`drawer${open ? " drawer--open" : ""}`} aria-label="Jobs">
      <header className="drawer__bar">
        <button className="drawer__toggle" onClick={() => onOpen(!open)} aria-expanded={open}>
          <span className={`drawer__chevron${open ? " drawer__chevron--open" : ""}`}>▸</span>
          Jobs
        </button>
        {!open && state.batches[0] && <Glance batch={state.batches[0]} />}
        <span className="drawer__spacer" />
        <label className="drawer__filter">
          <input
            type="checkbox"
            checked={showBackground}
            onChange={(e) => setShowBackground(e.target.checked)}
          />
          Background jobs
          {hidden > 0 && <span className="muted"> ({hidden} hidden)</span>}
        </label>
      </header>

      {open && (
        <div className="drawer__body">
          <div className="drawer__batches">
            {shown.length === 0 && <p className="muted drawer__idle">Nothing has run yet.</p>}
            {shown.map((batch) => (
              <Batch
                key={batch.id}
                batch={batch}
                selected={selected}
                onSelect={select}
                onCancel={() => onCancelBatch(batch)}
                onUndo={() => onUndoBatch(batch)}
                onRetry={(repo) => onRetry(batch, repo)}
              />
            ))}
          </div>
          {selected !== null && (
            <Transcript
              id={selected}
              lines={state.logs[selected] ?? []}
              onClose={() => setSelected(null)}
            />
          )}
        </div>
      )}
    </section>
  );
}

/** What the collapsed bar says, so the drawer can be shut without going blind. */
function Glance({ batch }: { batch: BatchView }) {
  const { done, total } = progress(batch);
  return (
    <span className="drawer__glance">
      {batch.label ?? `Batch ${batch.id}`}
      {batch.line ? ` — ${batch.line}` : ` — ${done}/${total}`}
    </span>
  );
}

function Batch({
  batch,
  selected,
  onSelect,
  onCancel,
  onUndo,
  onRetry,
}: {
  batch: BatchView;
  selected: JobId | null;
  onSelect: (id: JobId) => void;
  onCancel: () => void;
  onUndo: () => void;
  onRetry: (repo: RepoId) => void;
}) {
  const { done, total } = progress(batch);
  const live = isRunning(batch);
  return (
    <article className="batch">
      <header className="batch__head">
        <span className="batch__label">{batch.label ?? `Batch ${batch.id}`}</span>
        {live ? (
          <>
            <span className="batch__progress">
              {done}/{total}
            </span>
            <span className="batch__elapsed">{elapsed(Date.now() - batch.firstSeen)}</span>
            <button className="inline-action" onClick={onCancel}>
              Cancel
            </button>
          </>
        ) : (
          <>
            <span className="batch__summary">{batch.line}</span>
            <button className="inline-action" onClick={onUndo}>
              Undo…
            </button>
          </>
        )}
      </header>
      <ul className="batch__rows">
        {batch.rows.map((row) => (
          <Row
            key={row.id}
            row={row}
            selected={row.id === selected}
            onSelect={() => onSelect(row.id)}
            onRetry={() => onRetry(row.repo)}
            retryable={batch.action !== null}
          />
        ))}
      </ul>
    </article>
  );
}

function Row({
  row,
  selected,
  onSelect,
  onRetry,
  retryable,
}: {
  row: JobRow;
  selected: boolean;
  onSelect: () => void;
  onRetry: () => void;
  retryable: boolean;
}) {
  const kind = row.state.type.toLowerCase();
  return (
    <li className={`jobrow jobrow--${kind}${selected ? " jobrow--selected" : ""}`}>
      <button className="jobrow__open" onClick={onSelect} title={row.repo}>
        <span className="jobrow__spinner" aria-hidden="true" />
        <span className="jobrow__name">{basename(row.repo)}</span>
        <span className="jobrow__state">{stateLabel(row.state)}</span>
        {row.state.type === "Skipped" && (
          <span className="jobrow__why">{skipReason(row.state.value.why)}</span>
        )}
      </button>
      {row.state.type === "Failed" && (
        <button
          className="inline-action"
          onClick={onRetry}
          disabled={!retryable}
          title={retryable ? undefined : "This batch was not started from here."}
        >
          Retry this repo
        </button>
      )}
    </li>
  );
}

/** A skip reason as text: the `SkipReason` variant name, not a phrasing of it. */
function skipReason(why: { type: string }): string {
  return why.type.replace(/([a-z])([A-Z])/g, "$1 $2").toLowerCase();
}

/** The raw interleaved transcript. Monospaced and copyable, not parsed or summarised. */
function Transcript({
  id,
  lines,
  onClose,
}: {
  id: JobId;
  lines: LogLine[];
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);
  useEffect(() => setCopied(false), [id]);

  async function copy() {
    await navigator.clipboard.writeText(lines.map((l) => l.text).join("\n"));
    setCopied(true);
  }

  return (
    <aside className="transcript">
      <header className="transcript__head">
        <span>Transcript — job {id}</span>
        <span className="drawer__spacer" />
        <button className="inline-action" onClick={copy} disabled={lines.length === 0}>
          {copied ? "Copied" : "Copy"}
        </button>
        <button className="inline-action" onClick={onClose}>
          Close
        </button>
      </header>
      {lines.length === 0 ? (
        <p className="muted transcript__empty">No output.</p>
      ) : (
        <ol className="transcript__lines">
          {lines.map((line, i) => (
            <li key={i} className={`transcript__line transcript__line--${line.stream.toLowerCase()}`}>
              <span className="transcript__at">{time(line.at)}</span>
              <span className="transcript__text">{line.text}</span>
            </li>
          ))}
        </ol>
      )}
    </aside>
  );
}

/** `14:32:07.412` — millisecond resolution. */
function time(at: number): string {
  const d = new Date(at);
  const hms = d.toTimeString().slice(0, 8);
  return `${hms}.${String(d.getMilliseconds()).padStart(3, "0")}`;
}
