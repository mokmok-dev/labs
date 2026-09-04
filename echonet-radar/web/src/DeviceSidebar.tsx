import { useMemo } from "react";
import { Broadcast, Cpu, Stack } from "@phosphor-icons/react";
import { Empty, Meter, Sidebar, Text } from "@cloudflare/kumo";
import { sameDevice, summarizeDevices, type DeviceKey } from "./device";
import { MAX_EVENTS } from "./useRadarSocket";
import type { ChangePayload, Connection } from "./types";

interface DeviceSidebarProps {
  changes: ChangePayload[];
  connection: Connection;
  selected: DeviceKey | null;
  onSelect: (device: DeviceKey | null) => void;
}

export function DeviceSidebar({
  changes,
  connection,
  selected,
  onSelect,
}: DeviceSidebarProps) {
  const groups = useMemo(() => summarizeDevices(changes), [changes]);

  return (
    <Sidebar>
      <Sidebar.Header>
        <div className="radar-sidebar-brand">
          <Broadcast size={18} weight="duotone" />
          <Text variant="heading" as="h1">
            echonet-radar
          </Text>
        </div>
        <Sidebar.Trigger />
      </Sidebar.Header>
      {connection === "connecting" ? (
        <Sidebar.Loading />
      ) : groups.length === 0 ? (
        <Sidebar.Content>
          <Sidebar.Group>
            <Empty
              className="radar-sidebar-empty"
              size="sm"
              icon={<Broadcast size={32} className="text-kumo-inactive" />}
              title="No devices yet"
              description="Discovered ECHONET Lite objects will appear here."
            />
          </Sidebar.Group>
        </Sidebar.Content>
      ) : (
        <Sidebar.Content>
          <Sidebar.Group>
            <Sidebar.Menu>
              <Sidebar.MenuButton
                icon={Stack}
                active={selected === null}
                tooltip="All devices"
                onClick={() => onSelect(null)}
              >
                All devices
                <Sidebar.MenuBadge>{changes.length}</Sidebar.MenuBadge>
              </Sidebar.MenuButton>
            </Sidebar.Menu>
          </Sidebar.Group>
          {groups.map((group) => (
            <Sidebar.Group key={group.source}>
              <Sidebar.GroupLabel>{group.source}</Sidebar.GroupLabel>
              <Sidebar.Menu>
                {group.devices.map((device) => (
                  <Sidebar.MenuButton
                    key={`${device.source}/${device.eoj}`}
                    icon={Cpu}
                    active={selected !== null && sameDevice(selected, device)}
                    tooltip={`${device.eoj} — ${device.lastEdt}`}
                    onClick={() =>
                      onSelect({ source: device.source, eoj: device.eoj })
                    }
                  >
                    {device.eoj}
                    <Sidebar.MenuBadge>{device.events}</Sidebar.MenuBadge>
                  </Sidebar.MenuButton>
                ))}
              </Sidebar.Menu>
            </Sidebar.Group>
          ))}
        </Sidebar.Content>
      )}
      <Sidebar.Footer>
        <div className="radar-buffer">
          <Meter
            label="Event buffer"
            value={changes.length}
            max={MAX_EVENTS}
            customValue={`${changes.length} / ${MAX_EVENTS}`}
          />
        </div>
      </Sidebar.Footer>
    </Sidebar>
  );
}
