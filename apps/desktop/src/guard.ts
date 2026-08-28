/**
 * Whether a guarded plan may be confirmed yet.
 *
 * A pure function, separate from the sheet, for the same reason the two folds
 * are separate from their components: this decides whether a force-push across
 * forty repositories is allowed to start, and that is not something to leave
 * only readable inside a component.
 *
 * It composes no prose. The words a guard shows are `ConfirmGuard`'s own, from
 * `crates/engine`, which is why the CLI and the GUI cannot drift about what a
 * confirmation demands.
 */
import type { ConfirmGuard } from "./bindings";

/** What the user has supplied towards satisfying a guard. */
export type GuardInput = {
  /** What was typed into the count field. */
  typed: string;
  /** Whether the acknowledgement was ticked. */
  acknowledged: boolean;
};

export const NOTHING_SUPPLIED: GuardInput = { typed: "", acknowledged: false };

/**
 * May this plan run?
 *
 * `null` — no guard — is the ordinary case and is always satisfied: danger
 * styling and extra steps that appear on ordinary work are wallpaper within a
 * week, and then they are not there on the day they matter.
 */
export function satisfied(guard: ConfirmGuard | null, input: GuardInput): boolean {
  if (guard === null) return true;
  switch (guard.type) {
    case "TypeCount":
      // Exactly the number, and nothing else. Not `parseInt`, which reads
      // "3 repos" as 3 and would let a paste satisfy a check whose whole
      // purpose is that it cannot be satisfied without reading the plan.
      return input.typed.trim() === String(guard.value);
    case "Acknowledge":
      return input.acknowledged;
  }
}

/**
 * Is this plan one the user could get badly wrong?
 *
 * The presence of a guard *is* the danger signal — there is deliberately not a
 * second flag that could disagree with it.
 */
export function isDangerous(guard: ConfirmGuard | null): boolean {
  return guard !== null;
}
