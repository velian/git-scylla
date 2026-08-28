/** The configured roots, and the scan that follows a change to them. */
import { useCallback, useEffect, useState } from "react";
import { engine } from "./engine/client";
import type { Config } from "./bindings";
import type { Scan } from "./useScan";

const EMPTY: Config = { roots: [], editor: null, terminal: null, custom: [] };

export type Roots = {
  paths: string[];
  /** The whole persisted configuration, for the custom-command menu and settings editor. */
  config: Config;
  /** Replace it, after a command that returns the configuration as it now stands. */
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

  useEffect(() => {
    engine
      .getConfig()
      .then((config) => {
        setConfig(config);
        return rescan(config.roots);
      })
      .catch(onError);
  }, [rescan, onError]);

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
      if (picked === null) return;
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

  const adopt = useCallback((next: Config) => setConfig(next), []);

  return { paths, config, adopt, add, remove };
}
