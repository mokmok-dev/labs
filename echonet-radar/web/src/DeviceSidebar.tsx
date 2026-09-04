import { Broadcast, Cpu, Stack } from "@phosphor-icons/react";
import { Empty, Sidebar, Text } from "@cloudflare/kumo";
import { sameDevice, summarizeDevices, type DeviceKey } from "./device";
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
  const groups = summarizeDevices(changes);

  return (
    <Sidebar>
      <Sidebar.Header>
        <div className="radar-sidebar-brand">
          <Broadcast size={18} weight="duotone" />
          <Text variant="heading" as="h1">
            echonet-radar
          </Text>
        </div>
      </Sidebar.Header>
      {connection === "connecting" ? (
        <Sidebar.Loading />
      ) : groups.length === 0 ? (
        <Sidebar.Content>
          <Sidebar.Group>
            <Empty
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
        <Sidebar.Trigger />
      </Sidebar.Footer>
    </Sidebar>
  );
}
