import type { ChangePayload, DevicePayload } from "./types";

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

export function summarizeDevices(
  changes: ChangePayload[],
  devices: DevicePayload[],
): DeviceGroup[] {
  const groups = new Map<string, Map<string, DeviceSummary>>();
  for (const change of changes) {
    let group = groups.get(change.source);
    if (!group) {
      group = new Map();
      groups.set(change.source, group);
    }
    const existing = group.get(change.eoj);
    if (existing) {
      existing.events += 1;
    } else {
      group.set(change.eoj, {
        source: change.source,
        eoj: change.eoj,
        events: 1,
        lastActivityMs: change.atMs,
        lastEdt: change.edt,
      });
    }
  }
  // Devices discovered by the radar but silent so far appear immediately,
  // without waiting for their first observed value.
  for (const device of devices) {
    let group = groups.get(device.source);
    if (!group) {
      group = new Map();
      groups.set(device.source, group);
    }
    if (!group.has(device.eoj)) {
      group.set(device.eoj, {
        source: device.source,
        eoj: device.eoj,
        events: 0,
        lastActivityMs: 0,
        lastEdt: "",
      });
    }
  }
  return [...groups.entries()]
    .map(([source, group]) => ({
      source,
      devices: [...group.values()].sort(
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
