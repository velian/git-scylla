/**
 * The configured roots, and the scan that follows a change to them.
 *
 * The engine has no concept of a configured working set — it takes roots per
 * scan — so persisting the choice is the shell's job, and re-scanning after a
 * change is the other half of it. Kept together because a root added but not
 * scanned, or removed but still on screen, is the failure worth designing out.
 */
import { useCallback, useEffect, useState } from "react";
import { engine } from "./engine/client";
import type { Config } from "./bindings";
import type { Scan } from "./useScan";

const EMPTY: Config = { roots: [], editor: null, terminal: null, custom: [] };

export type Roots = {
  paths: string[];
  /** The whole persisted configuration, for the surfaces that need the rest of
      it — the custom-command menu and the settings editor. */
  config: Config;
  /** Replace it, after a command that returns the configuration as it now
      stands. */
  adopt: (config: Config) => void;
  /** Pick a directory and add it. A dismissed picker changes nothing. */
  add: () => Promise<void>;
  remove: (path: string) => Promise<void>;
};

export function useRoots(
  scan: Pick<Scan, "rescan" | "reset">,
  onError: (e: unknown) => void,
): Roots {
  const [config, setConfig] = useState<Config>(EMPTY);
  const paths = config.roots;
  const { rescan, reset } = scan;

  // Load the persisted roots and scan them, so a relaunch comes back to the
  // working set rather than to an empty window.
  useEffect(() => {
    engine
      .getConfig()
      .then((config) => {
        setConfig(config);
        return rescan(config.roots);
      })
      .catch(onError);
  }, [rescan, onError]);

  // The rows on screen belong to the old root set, so they go before the new
  // scan starts rather than lingering until it overwrites them.
  const rescanFor = useCallback(
    async (next: Config) => {
      setConfig(next);
      reset();
      await rescan(next.roots);
    },
    [rescan, reset],
  );

  const add = useCallback(async () => {
    try {
      const picked = await engine.pickRootDir();
      if (picked === null) return; // dismissed, which is a choice
      // The reply, not the argument: the merge rules may have dropped a nested
      // path or replaced narrower ones, and the window must not guess which.
      await rescanFor(await engine.addRoot(picked));
    } catch (e) {
      onError(e);
    }
  }, [rescanFor, onError]);

  const remove = useCallback(
    async (path: string) => {
      try {
        await rescanFor(await engine.removeRoot(path));
      } catch (e) {
        onError(e);
      }
    },
    [rescanFor, onError],
  );

  // Settings changes do not move the working set, so they replace the
  // configuration without triggering a scan.
  const adopt = useCallback((next: Config) => setConfig(next), []);

  return { paths, config, adopt, add, remove };
}
