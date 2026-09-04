import { useCallback, useEffect, useRef, useState } from "react";
import type { ChangePayload, Connection, ServerMessage } from "./types";

const MAX_EVENTS = 1000;
const RECONNECT_DELAY_MS = 1000;

const wsUrl = (): string => {
  const configured = import.meta.env.VITE_WS_URL;
  if (configured) return configured;
  const scheme = window.location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${window.location.host}/ws`;
};

interface RadarSocket {
  changes: ChangePayload[];
  status: string;
  connection: Connection;
  pollNow: () => void;
}

export function useRadarSocket(): RadarSocket {
  const [changes, setChanges] = useState<ChangePayload[]>([]);
  const [status, setStatus] = useState("connecting");
  const [connection, setConnection] = useState<Connection>("connecting");
  const [attempt, setAttempt] = useState(0);
  const socketRef = useRef<WebSocket | null>(null);

  const connect = useCallback(() => {
    const socket = new WebSocket(wsUrl());
    socketRef.current = socket;
    socket.onopen = () => setConnection("open");
    socket.onclose = () => {
      if (socketRef.current === socket) {
        socketRef.current = null;
        setConnection("closed");
        setAttempt((previous) => previous + 1);
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
    if (attempt === 0) return;
    const timer = window.setTimeout(connect, RECONNECT_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [attempt, connect]);

  const pollNow = useCallback(() => {
    if (socketRef.current?.readyState === WebSocket.OPEN) {
      socketRef.current.send(JSON.stringify({ type: "pollNow" }));
    }
  }, []);

  return { changes, status, connection, pollNow };
}
