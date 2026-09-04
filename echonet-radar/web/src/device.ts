import type { ChangePayload } from "./types";

export interface DeviceKey {
  source: string;
  eoj: string;
}

export interface DeviceSummary {
  source: string;
  eoj: string;
  events: number;
  lastActivityMs: number;
  lastEdt: string;
}

export interface DeviceGroup {
  source: string;
  devices: DeviceSummary[];
}

export function summarizeDevices(changes: ChangePayload[]): DeviceGroup[] {
  const groups = new Map<string, Map<string, DeviceSummary>>();
  for (const change of changes) {
    let devices = groups.get(change.source);
    if (!devices) {
      devices = new Map();
      groups.set(change.source, devices);
    }
    const existing = devices.get(change.eoj);
    if (existing) {
      existing.events += 1;
    } else {
      devices.set(change.eoj, {
        source: change.source,
        eoj: change.eoj,
        events: 1,
        lastActivityMs: change.atMs,
        lastEdt: change.edt,
      });
    }
  }
  return [...groups.entries()]
    .map(([source, devices]) => ({
      source,
      devices: [...devices.values()].sort(
        (a, b) => b.lastActivityMs - a.lastActivityMs,
      ),
    }))
    .sort((a, b) => {
      const aLatest = a.devices[0]?.lastActivityMs ?? 0;
      const bLatest = b.devices[0]?.lastActivityMs ?? 0;
      return bLatest - aLatest;
    });
}

export function latestState(changes: ChangePayload[]): ChangePayload[] {
  const seen = new Set<string>();
  return changes.filter((change) => {
    const key = `${change.source}|${change.eoj}|${change.epc}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

export function sameDevice(a: DeviceKey, b: DeviceKey): boolean {
  return a.source === b.source && a.eoj === b.eoj;
}
