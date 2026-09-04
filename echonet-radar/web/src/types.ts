export interface ChangePayload {
  atMs: number;
  source: string;
  eoj: string;
  epc: number;
  edt: string;
}

export type ServerMessage =
  | { type: "snapshot"; changes: ChangePayload[]; status: string }
  | { type: "change"; atMs: number; source: string; eoj: string; epc: number; edt: string }
  | { type: "status"; message: string };

export type Connection = "connecting" | "open" | "closed";
