import { Badge, Button, Table, Text } from "@cloudflare/kumo";
import { Broadcast, Repeat } from "@phosphor-icons/react";
import { useRadarSocket } from "./useRadarSocket";

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

function connectionBadge(connection: "connecting" | "open" | "closed") {
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
  const { changes, status, connection, pollNow } = useRadarSocket();

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
          <span className="radar-mono">
            <Text variant="mono-secondary">{changes.length} events</Text>
          </span>
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
                  <Text variant="mono-secondary">{formatTime(change.atMs)}</Text>
                </Table.Cell>
                <Table.Cell className="radar-col-source radar-mono">
                  <Text variant="mono-secondary">{change.source}</Text>
                </Table.Cell>
                <Table.Cell className="radar-col-eoj radar-mono">
                  <Text variant="mono">{change.eoj}</Text>
                </Table.Cell>
                <Table.Cell className="radar-col-epc radar-mono">
                  <Text variant="mono">{formatEpc(change.epc)}</Text>
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
