import { useCallback, useEffect, useRef, useState } from "react";
import { Badge, Button, Table, Text } from "@cloudflare/kumo";
import { Broadcast, Repeat } from "@phosphor-icons/react";

const MAX_EVENTS = 1000;

interface ChangePayload {
  atMs: number;
  source: string;
  eoj: string;
  epc: number;
  edt: string;
}

type ServerMessage =
  | { type: "snapshot"; changes: ChangePayload[]; status: string }
  | { type: "change"; atMs: number; source: string; eoj: string; epc: number; edt: string }
  | { type: "status"; message: string };

const wsUrl = (): string => {
  const configured = import.meta.env.VITE_WS_URL;
  if (configured) return configured;
  const scheme = window.location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${window.location.host}/ws`;
};

type Connection = "connecting" | "open" | "closed";

function formatTime(atMs: number): string {
  const time = new Date(atMs);
  const now = new Date();
  const sameDay =
    time.getFullYear() === now.getFullYear() &&
    time.getMonth() === now.getMonth() &&
    time.getDate() === now.getDate();
  const clock = time.toLocaleTimeString();
  return sameDay ? clock : `${time.toLocaleDateString()} ${clock}`;
}

function formatEpc(epc: number): string {
  return `0x${epc.toString(16).padStart(2, "0").toUpperCase()}`;
}

function connectionBadge(connection: Connection) {
  switch (connection) {
    case "open":
      return (
        <Badge variant="success" appearance="dot">
          live
        </Badge>
      );
    case "connecting":
      return <Badge variant="warning">connecting</Badge>;
    case "closed":
      return <Badge variant="error">reconnecting</Badge>;
  }
}

export function App() {
  const [changes, setChanges] = useState<ChangePayload[]>([]);
  const [status, setStatus] = useState("connecting");
  const [connection, setConnection] = useState<Connection>("connecting");
  const socketRef = useRef<WebSocket | null>(null);

  const connect = useCallback(() => {
    const socket = new WebSocket(wsUrl());
    socketRef.current = socket;
    socket.onopen = () => setConnection("open");
    socket.onclose = () => {
      if (socketRef.current === socket) {
        socketRef.current = null;
        setConnection("closed");
      }
    };
    socket.onmessage = (event) => {
      const message = JSON.parse(event.data) as ServerMessage;
      switch (message.type) {
        case "snapshot":
          setChanges(message.changes);
          setStatus(message.status);
          break;
        case "change":
          setChanges((previous) => [message, ...previous].slice(0, MAX_EVENTS));
          break;
        case "status":
          setStatus(message.message);
          break;
      }
    };
  }, []);

  useEffect(() => {
    connect();
    return () => {
      socketRef.current?.close();
      socketRef.current = null;
    };
  }, [connect]);

  useEffect(() => {
    if (connection !== "closed") return;
    const timer = window.setTimeout(connect, 1000);
    return () => window.clearTimeout(timer);
  }, [connection, connect]);

  const pollNow = useCallback(() => {
    if (socketRef.current?.readyState === WebSocket.OPEN) {
      socketRef.current.send(JSON.stringify({ type: "pollNow" }));
    }
  }, []);

  return (
    <div className="radar">
      <header className="radar-header">
        <div className="radar-title">
          <Broadcast size={18} weight="duotone" />
          <Text variant="heading" as="h1">
            echonet-radar
          </Text>
        </div>
        <div className="radar-status">
          {connectionBadge(connection)}
          <Text variant="secondary" size="sm">
            {status}
          </Text>
          <Text variant="mono-secondary" size="sm">
            {changes.length} events
          </Text>
          <Button
            variant="secondary"
            size="sm"
            icon={<Repeat size={14} />}
            onClick={pollNow}
          >
            Poll now
          </Button>
        </div>
      </header>

      <main className="radar-table">
        <Table>
          <Table.Header sticky>
            <Table.Row>
              <Table.Head className="radar-col-time">Time</Table.Head>
              <Table.Head className="radar-col-source">Source</Table.Head>
              <Table.Head className="radar-col-eoj">EOJ</Table.Head>
              <Table.Head className="radar-col-epc">EPC</Table.Head>
              <Table.Head>EDT</Table.Head>
            </Table.Row>
          </Table.Header>
          <Table.Body>
            {changes.map((change, index) => (
              <Table.Row key={`${change.atMs}-${index}`}>
                <Table.Cell className="radar-col-time radar-mono">
                  <Text variant="mono-secondary" size="sm">
                    {formatTime(change.atMs)}
                  </Text>
                </Table.Cell>
                <Table.Cell className="radar-col-source radar-mono">
                  <Text variant="mono-secondary" size="sm">
                    {change.source}
                  </Text>
                </Table.Cell>
                <Table.Cell className="radar-col-eoj radar-mono">
                  <Text variant="mono" size="sm">
                    {change.eoj}
                  </Text>
                </Table.Cell>
                <Table.Cell className="radar-col-epc radar-mono">
                  <Text variant="mono" size="sm">
                    {formatEpc(change.epc)}
                  </Text>
                </Table.Cell>
                <Table.Cell>
                  <Text size="sm">{change.edt}</Text>
                </Table.Cell>
              </Table.Row>
            ))}
          </Table.Body>
        </Table>
        {changes.length === 0 && (
          <div className="radar-empty">
            <Text variant="secondary">
              Waiting for ECHONET Lite device activity…
            </Text>
          </div>
        )}
      </main>
    </div>
  );
}