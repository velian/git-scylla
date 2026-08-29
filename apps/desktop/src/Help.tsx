/** The reference sidebar: hotkeys and the status column's glyph legend. */

const HOTKEYS: { keys: string; label: string }[][] = [
  [
    { keys: "↑ ↓", label: "Move the row cursor" },
    { keys: "Shift+↑ ↓", label: "Extend selection" },
    { keys: "Space", label: "Toggle selection at cursor" },
    { keys: "⌘A", label: "Select all" },
    { keys: "Esc", label: "Clear selection" },
  ],
  [
    { keys: "t", label: "Open highlighted row in Terminal" },
    { keys: "o", label: "Open highlighted row in editor" },
  ],
  [
    { keys: "⌘F", label: "Focus the filter" },
    { keys: "⌘R", label: "Refresh selection (or rescan)" },
    { keys: "⌘⇧R", label: "Rescan roots" },
    { keys: "⌘O", label: "Add root…" },
    { keys: "⌘J", label: "Toggle the jobs drawer" },
  ],
];

const STATUS_ICONS: { icon: string; label: string }[] = [
  { icon: "-", label: "No upstream" },
  { icon: "↑?  ↓?", label: "Upstream ref is gone or unreadable" },
  { icon: "↑N", label: "Commits ahead of upstream" },
  { icon: "↓N", label: "Commits behind upstream" },
  { icon: "●N", label: "Modified files" },
  { icon: "+N", label: "Staged files" },
  { icon: "?N", label: "Untracked files" },
  { icon: "×N", label: "Conflicted files" },
  { icon: "⚑N", label: "Stashes" },
  { icon: "bare", label: "No working tree" },
  { icon: "[op]", label: "Operation in progress (e.g. rebase, merge)" },
  { icon: "[timeout]", label: "The probe timed out" },
];

const FILTER_KEYS: { key: string; label: string }[] = [
  { key: "dirty", label: "Bare badge name (also: badge:dirty)" },
  { key: "badge:name", label: "conflict, in-progress, diverged, behind, ahead, dirty, staged, clean, unreachable, unknown" },
  { key: "branch:glob", label: "Branch name" },
  { key: "name:glob", label: "Repository name" },
  { key: "path:glob", label: "Path (~/ expands)" },
  { key: "kind:k", label: "normal, bare, worktree, submodule" },
  { key: "upstream:s", label: "none, gone, set, ahead, behind, diverged, ok" },
  { key: "op:o", label: "any, merge, rebase, cherry-pick, revert, bisect" },
  { key: "ahead:cmpN", label: "Commits ahead of upstream" },
  { key: "behind:cmpN", label: "Commits behind upstream" },
  { key: "staged:cmpN", label: "Staged file count" },
  { key: "modified:cmpN", label: "Modified file count" },
  { key: "untracked:cmpN", label: "Untracked file count" },
  { key: "conflicted:cmpN", label: "Conflicted file count" },
  { key: "stashes:cmpN", label: "Stash count" },
];

const FILTER_SYNTAX: { key: string; label: string }[] = [
  { key: "&", label: "Combine terms (AND)" },
  { key: "!", label: "Negate a term" },
  { key: "* ?", label: "Glob: any run / one character" },
  { key: "cmp", label: "> >= < <= = (default =) before a number" },
];

export function Help({ onClose }: { onClose: () => void }) {
  return (
    <aside className="help">
      <div className="help__head">
        <span>Help</span>
        <button className="sidebar__toggle" onClick={onClose} aria-label="Close help">
          ›
        </button>
      </div>
      <section className="help__section">
        <h3>Hotkeys</h3>
        {HOTKEYS.map((group, i) => (
          <dl key={i} className="help__list">
            {group.map(({ keys, label }) => (
              <div key={keys} className="help__row">
                <dt>
                  <kbd>{keys}</kbd>
                </dt>
                <dd>{label}</dd>
              </div>
            ))}
          </dl>
        ))}
      </section>
      <section className="help__section">
        <h3>Status icons</h3>
        <dl className="help__list">
          {STATUS_ICONS.map(({ icon, label }) => (
            <div key={icon} className="help__row">
              <dt>
                <kbd>{icon}</kbd>
              </dt>
              <dd>{label}</dd>
            </div>
          ))}
        </dl>
      </section>
      <section className="help__section">
        <h3>Filter verbs</h3>
        <dl className="help__list">
          {FILTER_KEYS.map(({ key, label }) => (
            <div key={key} className="help__row">
              <dt>
                <kbd>{key}</kbd>
              </dt>
              <dd>{label}</dd>
            </div>
          ))}
        </dl>
        <dl className="help__list">
          {FILTER_SYNTAX.map(({ key, label }) => (
            <div key={key} className="help__row">
              <dt>
                <kbd>{key}</kbd>
              </dt>
              <dd>{label}</dd>
            </div>
          ))}
        </dl>
      </section>
    </aside>
  );
}
