/**
 * The action bar.
 *
 * Every control here does one of two things: build an `Action` and ask for a
 * plan, or — for `Refresh`, which mutates nothing — go straight to the engine.
 * Neither branch decides whether a repository is eligible; that is the plan's
 * job, and this file has no opinion about it.
 *
 * Some actions need a message or a name first, which is what `Compose`
 * collects. That is a step *before* the plan and never a step around it: the
 * sheet is still the only path from an intention to a batch.
 */
import { useCallback, useRef, useState } from "react";
import { Compose, type Field, type Values } from "./Compose";
import { useDismiss } from "./useDismiss";
import type { Action, CustomCommand, Placeholder, PullMode } from "./bindings";

/** The modes, in the CLI's order: safest first. */
const PULL_MODES: { mode: PullMode; label: string }[] = [
  { mode: "FfOnly", label: "Fast-forward only" },
  { mode: "Rebase", label: "Rebase" },
  { mode: "Merge", label: "Merge" },
];

/** What a menu offers: either an action outright, or a form that makes one. */
type Choice =
  | { label: string; action: Action; danger?: boolean }
  | { label: string; compose: { title: string; submit: string; fields: Field[] };
      build: (v: Values) => Action; danger?: boolean };

export function ActionBar({
  count,
  busy,
  custom,
  placeholders,
  onAction,
  onRefresh,
}: {
  count: number;
  busy: boolean;
  /** Saved custom commands, from the shell's config. */
  custom: CustomCommand[];
  /** The template substitution set, from `core::template` — never restated
      here, because help that repeats a table is help that goes stale. */
  placeholders: Placeholder[];
  onAction: (action: Action) => void;
  onRefresh: () => void;
}) {
  // The reason, not just the disabled state. A greyed-out button that does not
  // say why reads as a broken one.
  const reason =
    count === 0
      ? "Select one or more repositories first."
      : busy
        ? "Working on the last request."
        : undefined;
  const off = reason !== undefined;

  const help =
    placeholders.length === 0
      ? undefined
      : `Placeholders: ${placeholders.map((p) => p.token).join(", ")}`;

  const commit: Choice = {
    label: "Commit…",
    compose: {
      title: "Commit",
      submit: "Plan the commit",
      fields: [
        { kind: "text", name: "message", label: "Message", placeholder: "chore({repo}): …", hint: help },
        {
          kind: "check",
          name: "stage_all",
          label: "Stage everything first",
          hint: " — `git add -A`, which includes untracked files",
        },
        {
          kind: "check",
          name: "no_verify",
          label: "Skip hooks",
          hint: " — a pre-commit that refuses a secret is doing its job",
        },
      ],
    },
    build: (v) => ({
      type: "Commit",
      value: {
        message: String(v.message),
        stage_all: Boolean(v.stage_all),
        no_verify: Boolean(v.no_verify),
      },
    }),
  };

  const pushes: Choice[] = [
    { label: "Push", action: { type: "Push", value: { set_upstream: null, force_with_lease: false } } },
    {
      label: "Push and set upstream",
      action: { type: "Push", value: { set_upstream: "origin", force_with_lease: false } },
    },
    {
      // Separated from the others by a rule, and marked. This is the one push
      // that can destroy somebody else's work.
      label: "Push with lease…",
      action: { type: "Push", value: { set_upstream: null, force_with_lease: true } },
      danger: true,
    },
  ];

  const branches: Choice[] = [
    {
      label: "Check out…",
      compose: {
        title: "Check out",
        submit: "Plan the checkout",
        fields: [
          { kind: "text", name: "rev", label: "Branch, tag or commit", placeholder: "main", hint: help },
          { kind: "check", name: "create", label: "Create it if it does not exist" },
        ],
      },
      build: (v) => ({
        type: "Checkout",
        value: { rev: String(v.rev), create: Boolean(v.create) },
      }),
    },
    {
      label: "New branch…",
      compose: {
        title: "New branch",
        submit: "Plan the branch",
        fields: [
          { kind: "text", name: "name", label: "Name", placeholder: "wip/{repo}", hint: help },
          { kind: "text", name: "from", label: "From (optional)", placeholder: "HEAD" },
        ],
      },
      build: (v) => ({
        type: "Branch",
        value: {
          name: String(v.name),
          from: String(v.from).trim() === "" ? null : String(v.from).trim(),
        },
      }),
    },
  ];

  // Derived per repository from that repository's tags, so nothing is asked
  // here except which series and where a new one starts. The name itself
  // appears in the plan, which is the only place it can honestly appear.
  const tags: Choice[] = (["dev", "rc"] as const).flatMap((channel) =>
    (["Minor", "Major"] as const).map((bump) => ({
      label: `Cut ${channel} tag (${bump.toLowerCase()} bump)`,
      action: {
        type: "DevTag",
        value: { channel, bump, name: null, push: "origin" },
      } as Action,
    })),
  );

  const stashes: Choice[] = [
    { label: "Stash", action: { type: "Stash", value: { include_untracked: false } } },
    {
      label: "Stash, including untracked",
      action: { type: "Stash", value: { include_untracked: true } },
    },
    { label: "Pop stash", action: { type: "StashPop" } },
  ];

  const customs: Choice[] = custom.map((c) => ({
    label: c.name,
    // Its own saved flags decide the semaphore and whether `head_before` is
    // recorded; the engine cannot reason about the command and does not try.
    action: {
      type: "Custom",
      value: { args: c.args, network: c.network, mutating: c.mutating },
    },
    danger: true,
  }));

  return (
    <div className="actionbar" role="toolbar" aria-label="Actions">
      <button
        disabled={off}
        title={reason}
        onClick={() => onAction({ type: "Fetch", value: { prune: false, tags: false } })}
      >
        Fetch
      </button>

      <Menu label="Pull" disabled={off} reason={reason} onAction={onAction}
        choices={PULL_MODES.map(({ mode, label }) => ({
          label,
          action: { type: "Pull", value: { mode } },
        }))}
      />
      {/* Fast-forward only, and no mode menu — deliberately narrower than the
          CLI's `sync-default --mode`. A default branch that cannot fast-forward
          has local commits on it, and reconciling those across a working set
          the user is not standing on is not a thing to do from a toolbar. The
          title says so rather than leaving the omission to be discovered. */}
      <button
        disabled={off}
        title={
          reason ??
          "Stash, switch to the default branch, pull, and put you back where you were. " +
            "Fast-forward only."
        }
        onClick={() => onAction({ type: "SyncDefault", value: { mode: "FfOnly", plan: null } })}
      >
        Sync default
      </button>

      <Menu label="Push" disabled={off} reason={reason} onAction={onAction} choices={pushes} />
      <Menu label="Commit" disabled={off} reason={reason} onAction={onAction} choices={[commit]} />
      <Menu label="Branch" disabled={off} reason={reason} onAction={onAction} choices={branches} />
      <Menu label="Stash" disabled={off} reason={reason} onAction={onAction} choices={stashes} />
      <Menu label="Tag" disabled={off} reason={reason} onAction={onAction} choices={tags} />
      {customs.length > 0 && (
        <Menu label="Custom" disabled={off} reason={reason} onAction={onAction} choices={customs} />
      )}

      {/* Not a plan: re-probing changes nothing, so a confirmation sheet would
          be asking permission to look. The same narrow exception `fetch_now`
          gets. */}
      <button disabled={off} title={reason} onClick={onRefresh}>
        Refresh
      </button>

      <span className="actionbar__reason">{reason ?? `${count} selected`}</span>
    </div>
  );
}

/** One split button and its menu, with an optional form behind a choice. */
function Menu({
  label,
  choices,
  disabled,
  reason,
  onAction,
}: {
  label: string;
  choices: Choice[];
  disabled: boolean;
  reason?: string;
  onAction: (action: Action) => void;
}) {
  const [open, setOpen] = useState(false);
  const [composing, setComposing] = useState<Choice | null>(null);
  const root = useRef<HTMLDivElement>(null);

  // A menu that survives a click elsewhere is a menu you have to fight.
  const dismiss = useCallback(() => {
    setOpen(false);
    setComposing(null);
  }, []);
  useDismiss(root, dismiss, open || composing !== null);

  function choose(choice: Choice) {
    setOpen(false);
    if ("action" in choice) {
      onAction(choice.action);
    } else {
      setComposing(choice);
    }
  }

  // A single choice needs no menu; the button is the choice.
  const single = choices.length === 1 ? choices[0] : null;

  return (
    <div className="actionbar__split" ref={root}>
      <button
        disabled={disabled}
        title={reason}
        aria-haspopup={single ? undefined : "menu"}
        aria-expanded={single ? undefined : open}
        onClick={() => (single ? choose(single) : setOpen((o) => !o))}
      >
        {label}
        {single ? "" : " ▾"}
      </button>
      {open && (
        <ul className="menu" role="menu">
          {choices.map((c) => (
            <li key={c.label} role="none">
              <button
                role="menuitem"
                className={c.danger ? "menu__danger" : undefined}
                onClick={() => choose(c)}
              >
                {c.label}
              </button>
            </li>
          ))}
        </ul>
      )}
      {composing && "compose" in composing && (
        <div className="menu menu--wide">
          <Compose
            title={composing.compose.title}
            submit={composing.compose.submit}
            fields={composing.compose.fields}
            onCancel={() => setComposing(null)}
            onSubmit={(values) => {
              setComposing(null);
              onAction(composing.build(values));
            }}
          />
        </div>
      )}
    </div>
  );
}
