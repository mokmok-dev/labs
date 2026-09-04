import { Broadcast, Repeat } from "@phosphor-icons/react";
import { Button, Empty, Loader, Table, Text } from "@cloudflare/kumo";
import type { ChangePayload } from "./types";

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

interface EventViewsProps {
  changes: ChangePayload[];
  loading: boolean;
  onPollNow: () => void;
}

export function EventViews({ changes, loading, onPollNow }: EventViewsProps) {
  return (
    <main className="radar-table">
      {loading && changes.length === 0 ? (
        <div className="radar-center">
          <Loader size="lg" />
          <Text variant="secondary">Connecting to echonet-radar…</Text>
        </div>
      ) : changes.length === 0 ? (
        <div className="radar-center">
          <Empty
            icon={<Broadcast size={48} className="text-kumo-inactive" />}
            title="No device activity yet"
            description="Events appear here when ECHONET Lite properties change."
            contents={
              <Button
                variant="secondary"
                icon={<Repeat size={14} />}
                onClick={onPollNow}
              >
                Poll now
              </Button>
            }
          />
        </div>
      ) : (
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
      )}
    </main>
  );
}
