/**
 * The custom-command editor.
 *
 * These are the deliberate escape hatch: the closed `Action` enum will not cover
 * everything, and the alternative is a shell loop with no plan, no transcript
 * and no per-repository results.
 *
 * The editor's job is to make what a definition *does not get* impossible to
 * miss. Preconditions do not apply, undo does not apply, and the engine has no
 * opinion about the command — so the acknowledgement is a first-class control
 * here rather than a line of small print.
 */
import { useEffect, useState } from "react";
import { engine } from "./engine/client";
import type { CustomCommand } from "./bindings";

export function Settings({
  custom,
  editor,
  terminal,
  onSave,
  onRemove,
  onSetEditor,
  onSetTerminal,
  onClose,
}: {
  custom: CustomCommand[];
  editor: string | null;
  terminal: string | null;
  onSave: (command: CustomCommand) => void;
  onRemove: (name: string) => void;
  onSetEditor: (editor: string | null) => void;
  onSetTerminal: (terminal: string | null) => void;
  onClose: () => void;
}) {
  const [draft, setDraft] = useState<CustomCommand | null>(null);

  // What the terminal handoff would actually use. Asked of the engine rather
  // than worked out here: the resolution reads `$TERM_PROGRAM` and the
  // filesystem, neither of which the front end can see, and a second
  // implementation of it would be a second thing to be wrong.
  const [resolved, setResolved] = useState<string | null>(null);
  useEffect(() => {
    engine.resolvedTerminal().then(setResolved).catch(() => setResolved(null));
  }, [terminal]);

  return (
    <div
      className="settings"
      role="dialog"
      aria-label="Settings"
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.stopPropagation();
          onClose();
        }
      }}
    >
      <header className="settings__head">
        <h2>Settings</h2>
        <button onClick={onClose}>Done</button>
      </header>

      <section>
        <h3>Editor</h3>
        <p className="muted">
          The application a repository opens in, by name as <code>open -a</code>{" "}
          understands it — “Visual Studio Code”, “Zed”. Left empty, <code>$EDITOR</code> is
          used, which for most people is a terminal program and will not work.
        </p>
        <input
          value={editor ?? ""}
          placeholder="Visual Studio Code"
          spellCheck={false}
          onChange={(e) => onSetEditor(e.target.value.trim() === "" ? null : e.target.value)}
        />
      </section>

      <section>
        <h3>Terminal</h3>
        <p className="muted">
          Where “Open in Terminal” opens, by name as <code>open -a</code>{" "}
          understands it. Left empty it is worked out: the terminal this was
          launched from, then whichever known one is installed, then Terminal.
        </p>
        <input
          value={terminal ?? ""}
          placeholder="worked out automatically"
          spellCheck={false}
          onChange={(e) =>
            onSetTerminal(e.target.value.trim() === "" ? null : e.target.value)
          }
        />
        {/* The resolved choice, shown because a handoff has no plan sheet to
            show it in — an automatic choice nobody can see before it is made is
            the kind this project does not make.

            Real text rather than the field's placeholder, and stated once. A
            placeholder carrying the answer would be greyed, easy to skim past,
            and would have to invent something to say on the one occasion the
            engine could not be asked. */}
        {terminal === null && resolved && (
          <small className="muted">
            currently <code>{resolved}</code>
          </small>
        )}
      </section>

      <section>
        <h3>Custom commands</h3>
        <p className="muted">
          An argv, never a shell string: there is no shell, no interpolation and
          nothing to escape. The engine has no opinion about what these do — no
          preconditions beyond the universal ones, and no undo.
        </p>

        <ul className="settings__customs">
          {custom.map((c) => (
            <li key={c.name}>
              <span className="settings__name">{c.name}</span>
              <code>git {c.args.join(" ")}</code>
              <span className="settings__flags">
                {c.network ? "network" : "local"}
                {c.mutating ? ", mutating" : ""}
                {/* The state that decides whether running it asks first. Shown,
                    because a user who cannot see it cannot understand why one
                    command asks and another does not. */}
                {c.acknowledged ? "" : ", not yet acknowledged"}
              </span>
              <button onClick={() => setDraft({ ...c })}>Edit</button>
              <button onClick={() => onRemove(c.name)} aria-label={`Remove ${c.name}`}>
                ×
              </button>
            </li>
          ))}
          {custom.length === 0 && <li className="muted">None yet.</li>}
        </ul>

        {draft === null ? (
          <button
            onClick={() =>
              setDraft({ name: "", args: [], network: true, mutating: true, acknowledged: false })
            }
          >
            Add a command…
          </button>
        ) : (
          <Editor
            draft={draft}
            onChange={setDraft}
            onCancel={() => setDraft(null)}
            onSave={() => {
              onSave(draft);
              setDraft(null);
            }}
          />
        )}
      </section>
    </div>
  );
}

function Editor({
  draft,
  onChange,
  onSave,
  onCancel,
}: {
  draft: CustomCommand;
  onChange: (next: CustomCommand) => void;
  onSave: () => void;
  onCancel: () => void;
}) {
  // The argv is edited as text and split on whitespace, which is the honest
  // shape for a field: a quoted-string parser here would be a shell in
  // everything but name, and this exists precisely because there is no shell.
  // An argument containing a space is the one case it cannot express, and the
  // rendered preview below is where that shows.
  const [args, setArgs] = useState(draft.args.join(" "));
  const argv = args.split(/\s+/).filter(Boolean);
  const ready = draft.name.trim() !== "" && argv.length > 0;

  return (
    <form
      className="settings__editor"
      onSubmit={(e) => {
        e.preventDefault();
        if (ready) {
          onChange({ ...draft, args: argv });
          onSave();
        }
      }}
    >
      <label>
        <span>Name</span>
        <input
          value={draft.name}
          placeholder="prune remotes"
          onChange={(e) => onChange({ ...draft, name: e.target.value })}
        />
      </label>
      <label>
        <span>Arguments</span>
        <input
          value={args}
          placeholder="remote prune origin"
          spellCheck={false}
          onChange={(e) => {
            setArgs(e.target.value);
            onChange({ ...draft, args: e.target.value.split(/\s+/).filter(Boolean) });
          }}
        />
        {/* Exactly what will run, which is the same three strings the plan
            sheet shows, the process gets and the transcript records. */}
        <small>
          will run <code>git {argv.join(" ")}</code>
        </small>
      </label>
      <label className="settings__check">
        <input
          type="checkbox"
          checked={draft.network}
          onChange={(e) => onChange({ ...draft, network: e.target.checked })}
        />
        <span>
          Reaches the network<small> — takes the network semaphore and its per-host cap</small>
        </span>
      </label>
      <label className="settings__check">
        <input
          type="checkbox"
          checked={draft.mutating}
          onChange={(e) => onChange({ ...draft, mutating: e.target.checked })}
        />
        <span>
          Can move HEAD<small> — records where the repository was, for the transcript</small>
        </span>
      </label>
      <div className="settings__actions">
        <button type="button" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className="primary" disabled={!ready}>
          Save
        </button>
      </div>
    </form>
  );
}
