import { useEffect, useState } from "react";
import type { FetchCell as FetchCellData, FetchStatus, RepoId } from "./bindings";
import { engine } from "./engine/client";

export function brief(ms: number): string {
  const secs = Math.max(0, Math.floor(ms / 1000));
  if (secs < 60) return `${secs}s`;
  if (secs < 5400) return `${Math.floor(secs / 60)}m`;
  return `${Math.floor(secs / 3600)}h`;
}

export function since(ms: number): string {
  const secs = Math.max(0, Math.floor(ms / 1000));
  if (secs < 90) return "just now";
  if (secs < 5400) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 172_800) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86_400)}d ago`;
}

export function fetchCellText(status: FetchStatus, now: number): string {
  switch (status.type) {
    case "NoRemote":
      return "no remote";
    case "Off":
      return "off";
    case "Quarantined":
      return "quarantined";
    case "BackingOff":
      return `retrying ${brief(status.value.until - now)}`;
    case "Fetched":
      return since(now - status.value.at);
    case "Never":
      return "never";
  }
}

function isLive(status: FetchStatus): boolean {
  return status.type === "Fetched" || status.type === "BackingOff";
}

const TICK_MS = 1000;

export function FetchCellView({
  id,
  cell,
  onError,
}: {
  id: RepoId;
  cell: FetchCellData;
  onError: (e: unknown) => void;
}) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!isLive(cell.status)) return;
    const timer = setInterval(() => setNow(Date.now()), TICK_MS);
    return () => clearInterval(timer);
  }, [cell.status.type]);

  return (
    <td className={`col-fetch ${cell.problem ? "is-problem" : ""}`} title={cell.detail ?? undefined}>
      {fetchCellText(cell.status, now)}
      {cell.problem && (
        <button
          className="inline-action"
          onClick={(e) => {
            e.stopPropagation();
            engine.fetchNow(id).catch(onError);
          }}
        >
          Fetch now
        </button>
      )}
    </td>
  );
}
