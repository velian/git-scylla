/**
 * Close a popover when the user clicks somewhere else or presses Escape.
 *
 * Both menus in this application had their own copy of this, and the copies
 * were not the interesting part — the two details underneath them are:
 *
 * * It listens on the **document**, not on the element that owns the menu. A
 *   menu is `position: fixed` and opaque, so one left open sits over whatever
 *   is beneath it and swallows those clicks; and every way of stranding one is
 *   a click somewhere the owner does not see — the toolbar, the sidebar, Clear.
 * * Escape is **stopped**. The window reads a loose Escape as "clear the
 *   selection", which would throw away the very thing the menu was about to act
 *   on.
 *
 * `onClose` is held in a ref, so a handler that closes over fresh state does
 * not mean rebinding the listeners on every render.
 */
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
