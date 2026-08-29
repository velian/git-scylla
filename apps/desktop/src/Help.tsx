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
    </aside>
  );
}
