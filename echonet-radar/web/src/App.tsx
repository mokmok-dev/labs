import { useMemo, useState } from "react";
import { Badge, Button, Sidebar, Text } from "@cloudflare/kumo";
import { Repeat } from "@phosphor-icons/react";
import { sameDevice, type DeviceKey } from "./device";
import { DeviceSidebar } from "./DeviceSidebar";
import { EventViews } from "./EventViews";
import { useRadarSocket } from "./useRadarSocket";

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
  const [selected, setSelected] = useState<DeviceKey | null>(null);

  const visible = useMemo(
    () =>
      selected === null
        ? changes
        : changes.filter((change) => sameDevice(selected, change)),
    [changes, selected],
  );

  return (
    <Sidebar.Provider defaultOpen collapsible="icon">
      <DeviceSidebar
        changes={changes}
        connection={connection}
        selected={selected}
        onSelect={setSelected}
      />
      <div className="radar-main">
        <header className="radar-header">
          <div className="radar-status">
            {connectionBadge(connection)}
            <Text variant="secondary" size="sm">
              {status}
            </Text>
            <span className="radar-mono">
              <Text variant="mono-secondary">{visible.length} events</Text>
            </span>
          </div>
          <Button
            variant="secondary"
            size="sm"
            icon={<Repeat size={14} />}
            onClick={pollNow}
          >
            Poll now
          </Button>
        </header>
        <EventViews
          changes={visible}
          loading={connection === "connecting"}
          onPollNow={pollNow}
        />
      </div>
    </Sidebar.Provider>
  );
}
