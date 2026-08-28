/** Whether a guarded plan may be confirmed yet. Composes no prose of its own — `ConfirmGuard`'s words come from `crates/engine`. */
import type { ConfirmGuard } from "./bindings";

/** What the user has supplied towards satisfying a guard. */
export type GuardInput = {
  typed: string;
  acknowledged: boolean;
};

export const NOTHING_SUPPLIED: GuardInput = { typed: "", acknowledged: false };

/** May this plan run? `null` — no guard — is always satisfied. */
export function satisfied(guard: ConfirmGuard | null, input: GuardInput): boolean {
  if (guard === null) return true;
  switch (guard.type) {
    case "TypeCount":
      return input.typed.trim() === String(guard.value);
    case "Acknowledge":
      return input.acknowledged;
  }
}

/** Is this plan one the user could get badly wrong? */
export function isDangerous(guard: ConfirmGuard | null): boolean {
  return guard !== null;
}
