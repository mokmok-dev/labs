import { useState } from "react";
import { Broadcast, MagnifyingGlass, Repeat } from "@phosphor-icons/react";
import {
  Button,
  Empty,
  InputGroup,
  Loader,
  Table,
  Tabs,
  Text,
  Toolbar,
} from "@cloudflare/kumo";
import { latestState } from "./device";
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

function matchesQuery(change: ChangePayload, query: string): boolean {
  if (query === "") return true;
  return (
    change.edt.toLowerCase().includes(query) ||
    change.source.toLowerCase().includes(query) ||
    change.eoj.toLowerCase().includes(query) ||
    formatEpc(change.epc).toLowerCase().includes(query)
  );
}

interface EventViewsProps {
  changes: ChangePayload[];
  loading: boolean;
  onPollNow: () => void;
}

export function EventViews({ changes, loading, onPollNow }: EventViewsProps) {
  const [view, setView] = useState("activity");
  const [query, setQuery] = useState("");

  const trimmed = query.trim().toLowerCase();
  const filtered = changes.filter((change) => matchesQuery(change, trimmed));
  const rows = view === "state" ? latestState(filtered) : filtered;

  return (
    <main className="radar-content">
      <div className="radar-toolbar">
        <Tabs
          variant="segmented"
          size="sm"
          value={view}
          onValueChange={setView}
          tabs={[
            { value: "activity", label: "Activity" },
            { value: "state", label: "State" },
          ]}
        />
        <Toolbar>
          <Toolbar.InputGroup
            aria-label="Search events"
            className="radar-search"
          >
            <InputGroup.Addon>
              <MagnifyingGlass size={14} />
            </InputGroup.Addon>
            <Toolbar.Input
              placeholder="Search events"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
            />
          </Toolbar.InputGroup>
        </Toolbar>
      </div>
      <div className="radar-table">
        {loading && changes.length === 0 ? (
          <div className="radar-center">
            <Loader size="lg" />
            <Text variant="secondary">Connecting to echonet-radar…</Text>
          </div>
        ) : changes.length > 0 && rows.length === 0 ? (
          <div className="radar-center">
            <Empty
              size="sm"
              title="No matching events"
              description="Try a different search query."
            />
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
                <Table.Head>
                  {view === "state" ? "Latest value" : "EDT"}
                </Table.Head>
              </Table.Row>
            </Table.Header>
            <Table.Body>
              {rows.map((change, index) => (
                <Table.Row key={`${change.atMs}-${index}`}>
                  <Table.Cell className="radar-col-time radar-mono">
                    <Text variant="mono-secondary">
                      {formatTime(change.atMs)}
                    </Text>
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
      </div>
    </main>
  );
}
