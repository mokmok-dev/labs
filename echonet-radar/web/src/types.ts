export interface ChangePayload {
  atMs: number;
  source: string;
  eoj: string;
  epc: number;
  edt: string;
}

export interface DevicePayload {
  source: string;
  eoj: string;
}

export type ServerMessage =
  | {
      type: "snapshot";
      changes: ChangePayload[];
      devices: DevicePayload[];
      status: string;
    }
  | { type: "change"; atMs: number; source: string; eoj: string; epc: number; edt: string }
  | { type: "device"; source: string; eoj: string }
  | { type: "status"; message: string };

export type Connection = "connecting" | "open" | "closed";
