/**
 * The confirmation sheet. Composes no prose of its own — headline, phrase,
 * reason, and button labels all arrive in the `PlanView` from
 * `crates/engine`, the same object the CLI renders as text. This file
 * decides layout only: modal, disclosure triangles, which control has focus.
 */
import { useEffect, useRef, useState } from "react";
import { relativeTo } from "./columns";
import { isDangerous, NOTHING_SUPPLIED, satisfied, type GuardInput } from "./guard";
import type { PlanRow, PlanSheet as Sheet, RepoId } from "./bindings";

export function PlanSheet({
  sheet,
  roots,
  onConfirm,
  onCancel,
}: {
  sheet: Sheet;
  roots: string[];
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  const { view } = sheet;
  const guard = view.confirm_guard;
  const [supplied, setSupplied] = useState<GuardInput>(NOTHING_SUPPLIED);
  const ready = satisfied(guard, supplied);

  useEffect(() => {
    const el = dialog.current;
    if (el && !el.open) el.showModal();
    // `showModal` runs its own focusing steps after mount and lands on the
    // first focusable element in the sheet; Cancel is focused explicitly
    // afterward so a stray Return dismisses rather than executes.
    cancel.current?.focus();
  }, []);

  // The dialog element's own Escape handling is unreliable across the
  // engines this ships on, so Escape is owned here: prevented and stopped so
  // the window-level handler does not also clear the selection.
  useEffect(() => {
    const el = dialog.current;
    if (!el) return;
    function onKey(e: KeyboardEvent) {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      onCancel();
    }
    el.addEventListener("keydown", onKey);
    return () => el.removeEventListener("keydown", onKey);
  }, [onCancel]);

  return (
    <dialog className={`sheet${isDangerous(guard) ? " sheet--danger" : ""}`} ref={dialog}>
      <h2 className="sheet__headline">{view.headline}</h2>
      {view.selection_note && <p className="sheet__note">{view.selection_note}</p>}

      <div className="sheet__rows">
        {view.eligible && <Row marker="✓" row={view.eligible} roots={roots} kind="go" />}
        {view.skips.map((row) => (
          <Row key={row.detail} marker="⏭" row={row} roots={roots} kind="skip" />
        ))}
      </div>

      {view.variants_note && (
        <div className="sheet__variants">
          <p>{view.variants_note}</p>
          <ul>
            {view.variants.map((v) => (
              <li key={v.command}>
                <span className="sheet__count">{v.label}</span>
                <code>{v.command}</code>
              </li>
            ))}
          </ul>
        </div>
      )}

      {view.warning && <p className="sheet__warning">{view.warning}</p>}

      {view.empty_note && <p className="sheet__empty">{view.empty_note}</p>}

      {guard && <Guard guard={guard} supplied={supplied} onChange={setSupplied} />}

      <footer className="sheet__actions">
        <button ref={cancel} onClick={onCancel}>
          Cancel
        </button>
        {view.confirm_label && (
          <button
            className={`primary${isDangerous(guard) ? " danger" : ""}`}
            disabled={!ready}
            onClick={onConfirm}
          >
            {view.confirm_label}
          </button>
        )}
      </footer>
    </dialog>
  );
}

function Guard({
  guard,
  supplied,
  onChange,
}: {
  guard: NonNullable<Sheet["view"]["confirm_guard"]>;
  supplied: GuardInput;
  onChange: (next: GuardInput) => void;
}) {
  if (guard.type === "TypeCount") {
    return (
      <div className="sheet__guard">
        <label htmlFor="guard-count">
          This cannot be undone. Type <strong>{guard.value}</strong> to confirm.
        </label>
        <input
          id="guard-count"
          className="sheet__guardinput"
          value={supplied.typed}
          inputMode="numeric"
          autoComplete="off"
          spellCheck={false}
          onChange={(e) => onChange({ ...supplied, typed: e.target.value })}
        />
      </div>
    );
  }
  return (
    <div className="sheet__guard">
      <label>
        <input
          type="checkbox"
          checked={supplied.acknowledged}
          onChange={(e) => onChange({ ...supplied, acknowledged: e.target.checked })}
        />{" "}
        {guard.value}
      </label>
    </div>
  );
}

/** One line of the plan, expandable to the repositories behind its count. */
function Row({
  marker,
  row,
  roots,
  kind,
}: {
  marker: string;
  row: PlanRow;
  roots: string[];
  kind: "go" | "skip";
}) {
  return (
    <details className={`planrow planrow--${kind}`}>
      <summary>
        {/* `display: grid` on `summary` suppresses the native disclosure marker. */}
        <span className="planrow__chevron" aria-hidden="true">
          ▸
        </span>
        <span className="planrow__marker">{marker}</span>
        <span className="planrow__count">{row.count}</span>
        <span className="planrow__phrase">{row.phrase}</span>
        <span className="planrow__detail">{row.detail}</span>
      </summary>
      <ul className="planrow__repos">
        {row.repos.map((id: RepoId) => (
          <li key={id} title={id}>
            {relativeTo(id, roots)}
          </li>
        ))}
      </ul>
    </details>
  );
}
