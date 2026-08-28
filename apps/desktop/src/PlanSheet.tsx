/**
 * The confirmation sheet.
 *
 * The rule this file exists to obey: **it composes no prose of its own.** Every
 * headline, phrase, reason and button label arrives in the `PlanView` from
 * `crates/engine`, which is the same object the CLI renders as text. If this
 * file phrased anything itself, the two surfaces would be free to disagree.
 *
 * What it does decide is layout — a modal, disclosure triangles, which control
 * has focus — because none of that is a claim about what will happen.
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

  // `showModal` rather than an overlay div, for the modality and the focus
  // containment. Not for Escape — see below.
  useEffect(() => {
    const el = dialog.current;
    if (el && !el.open) el.showModal();
    // Focused here rather than with `autoFocus`, which does not survive:
    // `showModal` runs its own focusing steps after mount and lands on the
    // first focusable thing in the sheet — one of the disclosure rows. Verified
    // by reading `document.activeElement` with the sheet open. Cancel takes
    // focus instead, so a stray Return dismisses rather than executes.
    cancel.current?.focus();
  }, []);

  // Escape, handled here rather than left to the dialog's own close request.
  //
  // Measured, not assumed: with the sheet open, Escape produced a `keydown` on
  // the dialog and no `cancel` or `close` at all, and calling `close()` by hand
  // closed the element while React went on holding the sheet in state. Either
  // half is a bug — an invisible sheet that still owns the app, or a sheet that
  // ignores Escape — and the app ships on WKWebView, which is not the engine
  // that was measured. Owning the key makes all three engines agree.
  useEffect(() => {
    const el = dialog.current;
    if (!el) return;
    function onKey(e: KeyboardEvent) {
      if (e.key !== "Escape") return;
      // Stopped, so the window-level handler does not also read it as "clear
      // the selection" and throw away what the sheet was about to act on.
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
          {/* The one thing the headline cannot say: what resolution actually
              produced. Both the heading and each row's label
              are composed in `crates/engine`, so this cannot drift from what
              the CLI prints — the label is a repository *name* when a command
              has only one, which is the normal shape for a derived tag. */}
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

      {/* What the action's own words cannot say about *these* repositories —
          the count of untracked files `add -A` would sweep up. Phrased
          in `crates/engine` like everything else here. */}
      {view.warning && <p className="sheet__warning">{view.warning}</p>}

      {view.empty_note && <p className="sheet__empty">{view.empty_note}</p>}

      {/* The obstacle, when there is one. Its words are the guard's, so the CLI
          demands the same thing in the same terms. */}
      {guard && <Guard guard={guard} supplied={supplied} onChange={setSupplied} />}

      <footer className="sheet__actions">
        {/* Default focus, so Return on a sheet nobody read does nothing. */}
        <button ref={cancel} onClick={onCancel}>
          Cancel
        </button>
        {/* Absent, not disabled, when there is nothing to run: `confirm_label`
            is `null` exactly then, so this cannot be got wrong here. */}
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

/**
 * The thing that must be done before the confirm control works.
 *
 * Composes no prose: the sentence is the guard's, and the only thing decided
 * here is which control collects the answer.
 */
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
        {/* An explicit chevron: `display: grid` on a `summary` suppresses the
            native disclosure marker, and a row that expands has to look like
            one or nobody will try. */}
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
