/** Closes a popover on an outside click or Escape. */
import { useEffect, useRef, type RefObject } from "react";

export function useDismiss(
  root: RefObject<HTMLElement | null>,
  onClose: () => void,
  /** Bind only while something is actually open. */
  active = true,
): void {
  const close = useRef(onClose);
  close.current = onClose;

  useEffect(() => {
    if (!active) return;
    function dismiss(e: MouseEvent) {
      if (!root.current?.contains(e.target as Node)) close.current();
    }
    function onKey(e: KeyboardEvent) {
      if (e.key !== "Escape") return;
      // Stopped so the window's own Escape handler does not also clear the selection.
      e.stopPropagation();
      close.current();
    }
    document.addEventListener("mousedown", dismiss);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", dismiss);
      document.removeEventListener("keydown", onKey);
    };
  }, [root, active]);
}
