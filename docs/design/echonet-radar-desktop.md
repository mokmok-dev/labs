# ECHONET Radar desktop app

Status: proposed for review. Date: 2026-10-29. Scope: read-only v1, English UI. macOS arm64 is the first verifiable target. This document covers design only. No production code is included.

## Summary

Build a native desktop app on the existing ECHONET Lite radar stack. The app listens on UDP 3610, shows discovered appliances and their live properties, and walks the user through the operating system's inbound network permission on macOS, Linux and Windows. The first release is read-only. It uses Rust and egui. The core keeps one owner of the UDP socket and publishes immutable snapshots to the UI. A separate permission layer reports what the OS firewall claims and what the LAN proves, and never collapses those two facts into one boolean.

## Problem and constraints

The terminal dashboard already proves the protocol work. A desktop app adds two things the terminal cannot provide: a window that stays open for non-terminal users, and a first-run flow for the OS network permission. Device control is out of scope for v1.

The existing stack imposes hard constraints.

- Port 3610 has one safe owner per process. `EchoNetSocket` binds a wildcard socket and an interface socket with `SO_REUSEADDR` and `SO_REUSEPORT`. A second listener on 3610 splits datagrams by 4-tuple hash, so the app runs one service and never opens a second socket.
- The event stream loses no state but has no snapshot. A consumer must fold `Change`, `Device`, `Seen` and `Status` events, and `Status(String)` is prose.
- `echonet-lite` exposes metadata and decode, but no class name. A device list needs a human-readable class name from the MRA tables.
- The workspace denies `unsafe_code`, `unwrap_used`, `expect_used` and `panic`. The Nix checks run clippy with `--deny warnings` and tests headless with no display and no multicast.
- Nix covers `x86_64-linux`, `aarch64-linux` and `aarch64-darwin`. Windows has no CI leg and no Nix path. Packaging today is a bare crane binary.

Read-only scope deletes the Set property map retention, the write command, the control model, the EDT encoder and the write tests. None of those are scaffolding for a later feature in this design. A future write feature gets its own design.

## Requirements review

The request maps to fewer features than it first appears, and one part is impossible on Linux as worded.

| Requirement | Decision |
|---|---|
| macOS, Linux, Windows | Keep. macOS targets Apple Silicon. Linux and macOS build through Nix. Windows gets a native cargo CI leg and its own installer path. |
| "3610 などのポート" | ECHONET Lite uses only UDP 3610 for this app. Keep one port. Add a second only when a real protocol needs one. |
| App-specific firewall opening on every OS | Keep the goal, correct the mechanism. Windows supports an app plus port rule. macOS prompts per app and the app cannot approve itself. Linux stock firewalls have no per-app inbound rule. The product promise becomes "one permission flow with an honest scope per OS". |
| Approve from inside the app | Windows and Linux can elevate a helper after an explicit click. macOS can only deep-link the settings pane; the OS dialog is the approval. |
| Rust with egui or gpui | Choose egui. Reasons are in the framework section. |
| Read-only first release | Keep. No write command, no controls, no Set map retention. |
| UI language | English only. No localization layer in v1. |
| Intel macOS | Out of scope. The app targets arm64. |
| Code signing | Apple Developer ID is available for macOS. Windows signing is not available yet, so the MSI and helper ship unsigned and rely on the install directory ACL. |
| Design first | This document. No code until the shape is agreed. |

The following are deliberately dropped.

- Multi-port generality. The permission request carries one port. If a future feature needs another, the type grows then.
- A privileged daemon. The helper runs only for the firewall action and exits. The app stays unprivileged.
- Device control. Writes, the Set map, typed editors, the EDT encoder and the operation UI are a later design.
- Japanese UI and Intel macOS. Both stay out of v1.

## Decisions

| Area | Decision |
|---|---|
| Framework | egui through `eframe`, default wgpu backend, no webview. |
| State ownership | One service thread owns the socket and the authoritative model. The UI reads immutable `Arc<RadarSnapshot>` values from a `watch` channel. |
| UI input | The UI sends `Command` values only. In v1 those are `PollNow` and `Rediscover`. It never touches a socket or wire byte. |
| Snapshot cadence | Publish on change, at most once per 100 ms, plus once when idle. Latest wins, so a slow frame cannot grow a queue. |
| Scope | Read-only discovery and display. No write path, no Set map, no control model, no EDT encoder. |
| Permissions | Separate crate with a capability table, an inspection report, a plan of plain argv, and a small elevated helper. Rule state and traffic evidence stay separate. |
| Readiness | A state machine over observations. Inbound traffic is the only proof that the LAN path works. |
| Platforms | macOS arm64, Linux x86_64 and aarch64, Windows x86_64. |
| Packaging | Nix for Linux and macOS, WiX v4 MSI for Windows, runtime helper for portable builds. macOS is signed and notarized. Windows is unsigned in v1. |

## Usage

The app's quickstart, as a user reads it.

```text
echonet-radar                 # open the window and pick an interface
echonet-radar --interface br0
echonet-radar --permission-inspect   # headless: print the firewall report
```

First run:

1. Pick the LAN interface. `Automatic` is the default.
2. Click **Start listening**. The service binds 3610 and sends one discovery request.
3. The OS may ask for permission. macOS asks about incoming connections and, on macOS 15 or later, local network access. Windows asks about the app or shows the rule button. Linux shows the port rule button.
4. The status moves to **Listening** only after a datagram arrives. If nothing arrives after ten seconds, the app shows the permission card with the next action and a **Check again** button.
5. Devices appear as they answer. Select a row to see values.

The main call sites, written before the types.

```rust
// main.rs
fn main() -> eframe::Result {
    let settings = Settings::load();
    let interfaces = net::enumerate();
    eframe::run_native(
        "ECHONET Radar",
        native_options(),
        Box::new(move |cc| {
            let app = RadarApp::new(settings, interfaces, cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    )
}
// The app does not bind at boot. The first-run card calls start() only after
// the user clicks Start listening, so the OS prompt arrives with context.
// main() parses --interface and --permission-inspect before settings and
// eframe. --permission-inspect prints the report as JSON and exits.
```

```rust
// app.rs, once per frame
impl eframe::App for RadarApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.running.is_none() {
            match views::start_card(
                ctx,
                &mut self.settings.interface,
                &self.interfaces,
                &mut self.start_action,
                self.start_failure.as_ref(),
            ) {
                StartAction::Start => self.start(ctx),
                StartAction::None => {},
            }
            return;
        }
        let Some(running) = self.running.as_mut() else {
            return;
        };
        if running.handle.has_changed() {
            self.snapshot = running.handle.snapshot();
        }
        self.permission.poll();
        self.readiness = readiness::step(&self.readiness, &self.snapshot);
        egui::CentralPanel::default().show(ctx, |ui| {
            views::status_bar(ui, &self.readiness);
            views::permission_card(ui, self.permission.report(), &mut self.actions);
            views::device_list(ui, &self.snapshot, &mut self.selection);
            if let Some(key) = self.selection {
                views::property_pane(ui, &self.snapshot, key);
            }
        });
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

impl RadarApp {
    fn start(&mut self, ctx: &egui::Context) {
        let interface = match net::resolve(&self.settings.interface) {
            Ok(interface) => interface,
            Err(missing) => {
                self.start_failure = Some(StartFailure::InterfaceMissing(missing));
                return;
            },
        };
        let config = SessionConfig {
            interface,
            discovery_interval: self.settings.discovery_interval,
            update_interval: self.settings.update_interval,
            ..SessionConfig::desktop()
        };
        match RadarSession::start(config, Arc::new(EguiPulse(ctx.clone()))) {
            Ok((session, handle)) => {
                self.running = Some(Running { session, handle });
                self.start_failure = None;
            },
            Err(fault) => self.start_failure = Some(StartFailure::Fault(fault)),
        }
    }
}

struct Running {
    session: RadarSession,
    handle: RadarHandle,
}

enum StartAction {
    None,
    Start,
}

struct EguiPulse(egui::Context);

impl Pulse for EguiPulse {
    fn pulse(&self) {
        self.0.request_repaint();
    }
}
```

```rust
// views/properties.rs, read-only
pub fn property_pane(ui: &mut egui::Ui, snapshot: &RadarSnapshot, key: ObjectKey) {
    let Some(object) = snapshot.object(key) else {
        return;
    };
    ui.heading(object.class_name);
    ui.label(format!("{} at {}", object.key.eoj, object.key.ip));
    for (epc, live) in &object.properties {
        ui.label(format!("0x{epc:02X}"));
        ui.label(&live.text);
    }
}
```

The call sites fix the vocabulary. The app imports `RadarSession`, `RadarHandle`, `RadarSnapshot`, `Command`, `ObjectKey`, `CommandError`, `Fault`, `SessionConfig`, `SessionStatus`, `StartFailure` and `Pulse` from the radar crates. It imports `Capability`, `Report`, `Verdict`, `Plan`, `Profiles`, `ApplyOutcome`, `ApplyError`, `apply` and `remove` from the permissions crate. It imports nothing from `echonet-lite` or `echonet-lite-udp`. `EguiPulse` is the app-side `Pulse` implementation.

## Architecture

### Module map

```text
echonet-lite/                 protocol codec and generated MRA tables
echonet-lite-udp/             UDP transport, unchanged
echonet-radar-core/           model, service and session
  src/lib.rs                  re-exports
  src/protocol.rs             frame parsing, instance lists, property maps
  src/format.rs               value formatting, union numbers, power helpers
  src/model.rs                RadarModel, RadarSnapshot, change records
  src/service.rs              timers, TIDs, pending requests, snapshot publishing
  src/session.rs              RadarSession, RadarHandle, Pulse, Fault
echonet-radar-permissions/    capability, inspection and elevation plans
  src/lib.rs                  public types and dispatch
  src/command.rs              CommandRunner trait and std implementation
  src/platform/{macos,windows,linux}.rs
echonet-radar-firewall-helper/  small elevated binary, separate package
echonet-radar/                the egui app
  src/main.rs                 settings, interface enumeration, eframe bootstrap
  src/app.rs                  RadarApp and screen routing
  src/readiness.rs            readiness state machine
  src/settings.rs             JSON settings, atomic write
  src/net.rs                  interface enumeration
  src/permission.rs           app-side permission controller
  src/views/{status,start,permission,devices,properties,settings}.rs
docs/design/echonet-radar-desktop.md
```

The app never imports `echonet-lite-udp`. The service never imports egui. The permissions crate never imports egui or the service. The helper never imports the app.

### Core types

```rust
/// Device identity. The port is excluded so a device that answers from an
/// ephemeral port stays one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey {
    pub ip: Ipv4Addr,
    pub eoj: Eoj,
}

/// One immutable view of the LAN. The service builds it. The UI only reads it.
pub struct RadarSnapshot {
    pub revision: u64,
    pub session: u64,
    pub status: SessionStatus,
    pub objects: BTreeMap<ObjectKey, ObjectSnapshot>,
    pub recent: Vec<ChangeRecord>,
    pub notices: Vec<Notice>,
    /// Every datagram this session accepted. Readiness and the status bar
    /// read these fields; the capped change ring cannot stand in for them.
    pub frames_seen: u64,
    pub last_frame: Option<SystemTime>,
}

pub struct ObjectSnapshot {
    pub key: ObjectKey,
    pub source: SocketAddr,
    pub class_code: u16,
    pub class_name: &'static str,
    pub last_seen: SystemTime,
    pub properties: BTreeMap<u8, PropertySnapshot>,
}

pub struct PropertySnapshot {
    pub raw_edt: Vec<u8>,
    pub text: String,
    pub at: SystemTime,
}

pub enum SessionStatus {
    Binding,
    Running { since: SystemTime },
    Degraded { fault: Fault, last_frame: Option<SystemTime> },
    Stopped,
}

pub enum Fault {
    Bind { interface: Ipv4Addr, detail: String },
    Config(ConfigError),
    Service(String),
}

/// A start failure the app can name. Interface resolution happens in the app
/// because settings store a name, not a lease-bound address.
pub enum StartFailure {
    InterfaceMissing(String),
    Fault(Fault),
}

pub struct ChangeRecord {
    pub at: SystemTime,
    pub key: ObjectKey,
    pub epc: u8,
    pub text: String,
    pub raw_edt: Vec<u8>,
}

pub struct Notice {
    pub at: SystemTime,
    pub text: String,
}

/// One parsed observation from the wire, before it becomes snapshot state.
/// The service builds these from frames; `RadarModel::observe` folds them.
pub enum Observation {
    Device { key: ObjectKey, class_code: u16 },
    Value {
        key: ObjectKey,
        epc: u8,
        raw_edt: Vec<u8>,
        text: String,
        at: SystemTime,
    },
    Seen { key: ObjectKey, at: SystemTime },
    Status(String),
}

pub enum Command {
    PollNow,
    Rediscover,
}

pub enum CommandError {
    ServiceStopped,
    Busy,
}
```

```rust
pub struct SessionConfig {
    pub interface: Ipv4Addr,
    pub discovery_interval: Duration,
    pub update_interval: Duration,
    pub request_timeout: Duration,
    pub snapshot_interval: Duration,
}

impl SessionConfig {
    /// Desktop defaults. The app starts from these and overrides intervals
    /// from settings.
    pub fn desktop() -> Self;
}

pub struct RadarSession {
    thread: JoinHandle<()>,
    shutdown: watch::Sender<bool>,
    // command sender lives in the handle; dropping it also stops the service
}

impl RadarSession {
    /// The socket is created inside the service runtime. Tokio registers socket
    /// readiness with the runtime that created it. The desktop constructor
    /// publishes snapshots only.
    pub fn start(config: SessionConfig, pulse: Arc<dyn Pulse>) -> Result<(Self, RadarHandle), Fault>;
    /// The CLI constructor adds the event projection for line modes.
    pub fn start_with_events(
        config: SessionConfig,
        events: Sender<RadarEvent>,
        pulse: Arc<dyn Pulse>,
    ) -> Result<(Self, RadarHandle), Fault>;
    pub fn alive(&self) -> bool;
    pub fn stop(self) -> Result<(), Fault>;
}

pub struct RadarHandle {
    snapshots: watch::Receiver<Arc<RadarSnapshot>>,
    commands: mpsc::Sender<Command>,
}

impl RadarHandle {
    pub fn has_changed(&self) -> bool;
    pub fn snapshot(&mut self) -> Arc<RadarSnapshot>;
    pub fn poll_now(&self) -> Result<(), CommandError>;
    pub fn rediscover(&self) -> Result<(), CommandError>;
}

pub trait Pulse: Send + Sync {
    fn pulse(&self);
}
```

`Pulse` keeps egui out of core. The app implements it over `egui::Context::request_repaint`. Tests use `NoopPulse`.

### Snapshot publication

The service keeps a `RadarModel` and folds each observation into it once.

```rust
impl RadarModel {
    fn observe(&mut self, observation: Observation) -> Projection;
    fn snapshot(&self) -> RadarSnapshot;
}

pub struct Projection {
    pub event: Option<RadarEvent>,
    pub dirty: bool,
}
```

`run_service` publishes `Arc::new(model.snapshot())` when dirty, at most once per `snapshot_interval`, and once after any state change that ends a burst. `revision` increases on every publish. The UI checks `has_changed`, clones the `Arc`, and renders. A missed publish costs nothing because the next one contains all state.

The CLI keeps the event projection for its line modes. The dashboard can migrate to snapshots and delete its private fold. Both projections come from one `observe` call, so the two renderers cannot disagree about the domain.

### Session and threading

```text
main thread                          service thread
┌────────────────────────────┐       ┌─────────────────────────────────┐
│ eframe event loop          │       │ current_thread tokio runtime     │
│  reads Arc<RadarSnapshot> ◄─────────┤ watch<Arc<RadarSnapshot>>       │
│  sends Command            ─────────►│ mpsc<Command>                    │
│  implements Pulse         ◄─────────┤ pulse() after publish            │
└────────────────────────────┘       │ socket bound inside block_on     │
                                      └─────────────────────────────────┘
```

The session starts the thread, builds the runtime, binds the socket inside `block_on`, reports a bind failure as `Fault`, and then runs the service loop. `stop` sends `true` on the shutdown watch, drops the command sender, and joins. A `Drop` implementation signals shutdown only, so a panic path cannot deadlock.

A socket-affecting settings change stops the session and starts a new one. Overlapping sockets on 3610 silently lose frames, so restart is sequential and idempotent. `session` changes on restart, and the UI clears the selection.

### Settings and interface choice

```rust
pub struct Settings {
    pub schema: u32,
    pub interface: InterfaceChoice,
    pub discovery_interval: Duration,
    pub update_interval: Duration,
    pub selected: Option<ObjectKey>,
}

pub enum InterfaceChoice {
    Auto,
    Named(String),
}
```

Settings store an interface name, not an address, so a DHCP renewal does not persist a stale lease. `net::resolve` turns the choice into an `Ipv4Addr` when the session starts. `Named` that no longer exists returns `StartFailure::InterfaceMissing(name)` and reopens the picker. `Auto` returns `Ipv4Addr::UNSPECIFIED`.

`Settings::load` is total. A missing file, a corrupt file, or a newer schema yields defaults plus a startup notice. The app keeps startup notices in its own list and shows them beside the snapshot notices once the session starts. Load clamps discovery to 10 through 3600 seconds and update polling to 5 through 600 seconds, so a hand-edited file cannot create a busy loop. The service still validates zero intervals at its boundary.

The two persistence owners are explicit. eframe owns window geometry. `settings.json` owns everything in the struct above and nothing else. Writes are atomic (temporary file then rename), debounced by one second, and flushed on exit. `selected` is cleared when the first snapshot after a restart does not contain it.

### Read-only scope

The first release observes. It does not write.

`Command` carries `PollNow` and `Rediscover`. The service sends discovery GETs, polls the properties in each device's Get map, processes INF telegrams, and publishes snapshots. It never sends `SetC`.

The deleted pieces are deliberate.

- No `0x9E` retention. The service still requests the three maps during discovery as it does today, and it keeps only `0x9F` for polling.
- No write command, pending SetC request, write outcome, or write ring.
- No control model, editor state, `encode_into`, `TypedValue`, `lookup_inherited`, or state-variant table.
- No write UI. The property pane renders `PropertySnapshot.text`; the device list renders `ObjectSnapshot.class_name`.

The only `echonet-lite` addition is `class_name(class) -> &'static str`. The MRA JSON carries `className.en`, and the IR already stores `class_name_en`. The generator emits a lookup table from that English name. No other language table is emitted in v1.

A future write feature needs its own design. It would reintroduce the Set map, validation, the encoder and the editor model, and it would need the service socket for the reasons above. None of that is built now.

The CLI keeps its one-shot write mode. Do not run the CLI write mode or the CLI dashboard beside the desktop app. Both processes bind 3610 and the kernel splits datagrams by `SO_REUSEPORT`.

## Permissions

### Platform matrix

| Host | Rule scope | App can add | App can read back | What the app does |
|---|---|---|---|---|
| macOS 14 and earlier, Application Firewall on | App, by signature | No | No | Explains the OS prompt, opens Firewall settings, proves with traffic |
| macOS 15 and later | App, plus Local Network permission | No | No | Ships `NSLocalNetworkUsageDescription`, opens the Privacy pane, proves with traffic |
| Windows, MSI install | App plus UDP 3610 on private and domain, two rules | Yes, at install | Yes, with filtered read-back | Reads the rule back, reports `RulePresent` only when program, port, protocol, action and profile all match |
| Windows, portable build under `%ProgramFiles%`, no rule | App plus UDP 3610 | Yes, UAC helper | Yes, when the account permits the read | Offers **Add firewall rule**, re-reads until the matching rule is present |
| Windows, portable build outside `%ProgramFiles%` | App plus UDP 3610 | No, manual steps | Yes, when the account permits the read | Shows the command; does not elevate a user-writable helper |
| Linux, firewalld | Port only, default zone | Yes, polkit helper, runtime and permanent | No, `query-port` needs auth | Labels the rule "port 3610 for this machine"; traffic proves the path |
| Linux, ufw | Port only | Yes, polkit helper | No, `ufw status` needs root | Same label; traffic proves the path |
| Linux, no supported backend | Unknown | No | No | `ManualSteps`, with the command for the user's firewall |
| NixOS | Declarative | No | No | Shows the `allowedUDPPorts` snippet |
| Flatpak or Snap | Sandboxed network | No | No | `Unsupported`, with the manifest change |

The table is the product contract. The UI selects the card sentence from `Verdict` and uses `Capability.scope` only when the app can add a rule. It never calls a Linux rule app-specific. macOS builds target arm64 only. Intel Macs can run the CLI, not this app.

The app adds private and domain rules. It never adds a Public rule in v1. A user whose home LAN is classified Public gets a warning and the manual command. The type surface, the helper and the MSI all stay at two rules.

### Capability and report

```rust
pub enum Scope {
    Application,
    ApplicationAndPort,
    Port,
}

pub struct Capability {
    pub scope: Scope,
    pub can_add_rule: bool,
    pub can_read_rule: bool,
    pub needs_helper: bool,
    pub notes: &'static str,
}

pub struct Request {
    /// The app path used for the unprivileged read-back only.
    pub target: PathBuf,
    pub port: u16,
    pub profiles: Profiles, // Windows private and domain by default
}

pub enum Verdict {
    NotInspected,
    /// Windows only, when every profile reports the firewall disabled.
    NoFirewall,
    RulePresent { method: Method, scope: Scope },
    RuleAbsent,
    NotObservable { backend: BackendState },
    ManualSteps { case: ManualCase, steps: &'static str },
    Unsupported { reason: &'static str },
}

pub enum Method {
    /// The only readable rule source in v1. macOS, firewalld and ufw
    /// return `NotObservable`.
    WindowsDefender,
}

pub enum BackendState {
    Running { permanent: Option<bool> },
    Inactive,
    Absent,
    NotObservable,
}

pub enum ManualCase {
    NixOs,
    Flatpak,
    Snap,
    NoSupportedBackend,
    UfwInactive,
    UntrustedHelper,
}

pub struct Profiles {
    pub private: bool,
    pub domain: bool,
}

pub struct Report {
    pub verdict: Verdict,
    pub checked_at: SystemTime,
}

pub enum ApplyOutcome {
    Applied,
    Cancelled,
    ManualSteps,
}

pub struct Plan {
    /// The helper to run. Its path must pass the trusted-prefix check.
    pub helper: PathBuf,
    /// `--add` or `--remove`, a backend from a closed enum, and port 3610.
    /// The target application is never an argument; the helper derives it
    /// from its own directory.
    pub args: Vec<OsString>,
    pub scope: Scope,
    pub manual: String,
}

pub fn capability() -> Capability;
pub fn inspect(request: &Request) -> Report;
pub fn plan(request: &Request) -> Plan;
pub fn apply(plan: &Plan) -> Result<ApplyOutcome, ApplyError>;
pub fn remove(request: &Request) -> Result<ApplyOutcome, ApplyError>;

pub enum ApplyError {
    UntrustedHelper,
    NotFound,
    NotAuthorized,
    Failed(String),
}
```

`inspect` shells out to system tools and parses exit codes. `Plan` is plain data. The helper receives only a closed command, a backend and the fixed port. It derives the target application from its own directory, so no caller-supplied program path crosses the elevation boundary. The exact argv is testable on every host.

One app-side `PermissionController` worker owns `inspect`, `plan`, `apply` and the re-inspect poll. The UI sends an action and polls `report()` and `last_outcome()`. `apply` never runs on the egui thread, because UAC and polkit can block for many seconds. A cancelled prompt is `ApplyOutcome::Cancelled`, not a failed action, and the card falls back to the manual steps.

Inspection details:

- Windows: one fixed PowerShell script returns JSON, or exit code 2 on an access error. The script reads the rule by stable `-Name` from `PersistentStore`, then checks `Enabled`, `Direction=Inbound`, `Action=Allow`, the port filter (`Protocol=UDP`, `LocalPort=3610`), the application filter (`Program` equals the app path), and the rule profile against `Get-NetConnectionProfile`. Exit 0 means every field matches, 1 means absent, 2 means the read failed. The script carries no user input. Verify the standard-user read when the Windows host is available. The exit codes are unit-tested with fixture outputs in unit 7.
- Linux firewalld: `firewall-cmd --state` is allowed unprivileged. The shipped policy requires admin auth for `firewall-cmd --query-port=3610/udp` on stock Fedora and RHEL; the exact action is a spike detail. The app does not call it. It reports `NotObservable { backend: Running { permanent: None } }` and relies on traffic. `firewall-cmd --get-active-zones` is unprivileged and names the zone the rule would use.
- Linux ufw: `systemctl is-active ufw` and `command -v ufw` detect the backend. `ufw status` needs root, so the app reports `NotObservable` and does not add a rule while ufw is inactive.
- Linux without firewalld or ufw: report `ManualSteps`, not `NoFirewall`. Plain nftables, iptables, Docker and a stopped firewalld all remain possible. `NoFirewall` is not a claim the app can make from two missing binaries.
- NixOS: `/etc/NIXOS` exists. Return `ManualSteps` with the snippet.
- Flatpak or Snap: `/.flatpak-info` or `SNAP` is set. Return `Unsupported`.
- macOS: return `NotObservable`. There is no supported API for the per-app Application Firewall verdict, and the app does not run the undocumented `socketfilterfw` queries.

### Readiness state machine

```rust
pub enum Readiness {
    NotStarted,
    Binding,
    Unverified { since: SystemTime },
    Listening { since: SystemTime, last: SystemTime, frames: u64 },
    Degraded { fault: Fault, last_frame: Option<SystemTime> },
    Stopped,
}

pub enum Freshness {
    Fresh,
    Stale,
    Lost,
}

/// Polls missed before a device's row is muted, then marked lost.
pub const STALE_AFTER_POLLS: u32 = 3;
pub const LOST_AFTER_POLLS: u32 = 5;

/// Total and idempotent. The permission report is not an input: a rule never
/// proves the LAN path.
pub fn step(state: &Readiness, snapshot: &RadarSnapshot) -> Readiness;

pub fn freshness(last_seen: SystemTime, now: SystemTime, update_interval: Duration) -> Freshness;
```

| Snapshot status | Readiness |
|---|---|
| `SessionStatus::Binding` | `Binding` |
| `SessionStatus::Running` with `last_frame == None` | `Unverified { since }` |
| `SessionStatus::Running` with `last_frame == Some(last)` | `Listening { since, last, frames: frames_seen }` |
| `SessionStatus::Degraded { fault, last_frame }` | `Degraded { fault, last_frame }` |
| `SessionStatus::Stopped` | `Stopped` |

The app calls `step` once per frame with the latest snapshot. A restart resets the readiness to `Binding` through the new snapshot's status. `Readiness::NotStarted` is the state before the user clicks **Start listening**.

`freshness` is separate from `step`, because a device can be stale while the session is listening. It reads `last_seen`, the current time and the configured update interval. The status bar and the device rows use its three values. The thresholds live here, not in view prose, so unit 6 can test them.

A permission report never moves the state to `Listening`. Only a received datagram does.

### First-run flow

**macOS.** The window opens on a card with one button, **Start listening**, and one sentence naming the OS dialog that follows. Clicking it starts the session. The Application Firewall dialog may appear if the firewall is on; UDP and `SO_REUSEPORT` behavior is part of the spike. On macOS 15 or later, the Local Network dialog appears on the first multicast access for an app launched from Finder. After ten seconds without a frame, the card explains that macOS cannot scope the rule to port 3610, offers **Open Firewall settings** and **Open Local Network settings**, and notes that an unsigned or ad-hoc rebuild may be treated as a new app by Local Network permission.

**Windows.** The app offers two paths. **Add firewall rule** asks for confirmation, shows the exact rule (program, UDP 3610, private and domain profiles), then starts the helper, which triggers UAC. After the helper exits, the app re-reads both rules once per second for five seconds. It accepts the rule only when the program, UDP 3610, inbound allow and profile all match. A matching rule still leaves the status at `Unverified` until traffic arrives. **Let Windows ask** starts listening and waits for the OS notification. A Public network profile does not match the default rule. The app says so and shows the manual command; it does not add a Public rule in v1.

**Linux.** The app probes Flatpak, Snap, NixOS, firewalld and ufw in that order. A running firewalld or ufw gets an **Allow UDP 3610** button that runs the helper through `pkexec`. An inactive or absent backend gets `ManualSteps`, not a button, because adding a rule to a stopped firewall proves nothing. The firewalld helper writes both the runtime and the permanent configuration, then reloads; the card names the default zone that `--get-active-zones` reports. A cancelled or unavailable polkit prompt becomes `ManualSteps`, not an error toast. The card always says the rule covers the machine. NixOS gets the `networking.firewall.allowedUDPPorts` snippet. Flatpak and Snap get `Unsupported` and the manifest change.

### Helper boundary

The helper is a separate package, `echonet-radar-firewall-helper`, so the elevated surface stays out of the GUI binary.

- Windows: the binary embeds a manifest with `requestedExecutionLevel level="requireAdministrator"`. Launching it shows UAC. It derives the target `echonet-radar.exe` from its own directory, so no program path crosses the elevation boundary. It writes two rules, `EchonetRadarUdp3610Private` and `EchonetRadarUdp3610Domain`, each with the app path, UDP 3610, inbound allow. It calls `powershell -NoProfile -NonInteractive -Command` with a fixed script and passes the app path in an environment variable, so no path is interpolated into the script. `netsh` is not used because it reparses the command line. The fixed names make removal and read-back unambiguous. The helper checks that it and the target sit under `%ProgramFiles%`, that the directory is writable only by administrators, and that the target is a regular file rather than a reparse point. Windows signing is not available in v1, so the check is path and ACL based rather than signature based. A portable build outside `%ProgramFiles%` falls back to `ManualSteps`.
- Linux: `pkexec` runs the helper as root. The helper accepts only `--add` or `--remove`, port 3610, and a backend from a closed enum (`firewalld` or `ufw`). It runs `firewall-cmd` or `ufw` with fixed arguments. firewalld gets `--permanent --add-port=3610/udp` followed by `--reload`, so the rule survives a reboot. ufw rules are persistent by default. The helper refuses when ufw is inactive. Raw nftables is not a backend: a new accept chain cannot override a later drop in a distro-owned base chain. `pkexec` does not validate the helper path or its arguments, so the app and the helper both enforce a trusted prefix (`/nix/store`, `/usr`, `/opt`, `/run/current-system/sw`), an owner of root, and no group or other write permission. A development build outside those prefixes falls back to `ManualSteps`.
- macOS: no helper. `apply` opens System Settings and returns.

Every helper action is idempotent. Windows deletes and re-adds by name. firewalld `--add-port` and ufw `allow` are idempotent. Removal treats a missing rule as success.

Exit codes map to `ApplyOutcome`, not to a generic error. A cancelled UAC prompt (Windows error 1223) is `Cancelled`. firewalld `NOT_ENABLED` (12) and a missing rule on remove are success. firewalld `NOT_AUTHORIZED` (253) is `Cancelled` with the manual steps. Anything else is `Failed` with the tool's stderr in the diagnostics view.

The app changes firewall state only after an explicit click and a confirmation that shows the scope and the command. It never adds a rule on startup. The polkit action uses `auth_admin`, so a cached `allow_active` grant cannot skip the prompt. The helper re-checks its own path and the target path before acting, because `pkexec` validates neither.

### What the app never claims

- It never says the firewall is open because a rule exists. Only inbound traffic proves the path.
- It never calls a Linux port rule app-specific.
- It never claims macOS, firewalld or ufw expose a readable firewall verdict.
- It never changes a device property. The first release is read-only.
- It calls the helper only from an explicit confirmation and logs the exact action. The OS authentication dialog is the gate, so the claim is a product policy, not a technical guarantee.

## UI

### Screens

**Status bar.** Interface, readiness, last frame age, device count, a manual poll button, a rediscover button, and a **Network access** button. Device rows use `freshness` with the thresholds above. The session readiness is independent of per-device staleness.

**Permission card.** Shown while readiness is `Unverified` or `Degraded`, or the verdict is `Unsupported` or `ManualSteps`. One primary action per platform, a **Check again** button, and a short explanation of what the OS just asked or will ask.

**Device list.** One row per object: class name, EOJ, address, key value, last seen. Sorted by class name then IP. Selection is stable across snapshots through `ObjectKey`. A row for a stale device stays visible with a muted timestamp; there is no eviction in core.

**Property pane.** Live values grouped by property name. The pane is read-only. Each row shows the EPC, the formatted value and the time of the last change. A property that has not been observed yet is absent.

**Settings and diagnostics.** Interface picker, discovery and poll intervals, snapshot log export, firewall report, **Add rule**, **Remove rule**, and the helper's last command. The diagnostics show the multicast group, the bound addresses (`0.0.0.0:3610` and the interface address), and the last error.

### Interaction details

- Values render from `PropertySnapshot.text`, which core produces with `format_edt` and the generated metadata.
- The power chart samples once per second, cap 120 samples, and centers on zero for buy and sell. The series is computed in the app from `PropertySnapshot`, not in core.
- The window remembers size and position through eframe persistence. Settings use a separate JSON file so the interface is known before the window opens.
- A vanished interface name falls back to `Automatic` with a notice. A bind failure shows `Degraded` with **Restart**; there is no silent retry loop.
- Keyboard: arrow keys move the device selection, Enter opens the property pane, `p` polls, `r` retries the session. These mirror the TUI where the action exists.

### Language

The app ships English UI copy and the English class and property names from the MRA tables. This is the whole language plan. There is no localization layer, no CJK font and no Japanese name table in v1.

### Libraries

- `eframe` with the default wgpu backend and `accesskit`.
- `egui_extras` for the device table.
- `egui_plot` for the power chart.
- `if-addrs` for interface enumeration.
- `directories` for the settings path.
- `serde` and `serde_json` for settings.
- `tracing` and `tracing-subscriber` for logs, written to the platform log directory.

The app crate owns these. Core gains no GUI dependency. The permissions crate uses `std::process::Command` only.

## Framework decision

| Criterion | egui and eframe | gpui |
|---|---|---|
| Published release | `eframe` 0.37, published 2026-10-15 | `gpui` 0.2.2 on crates.io, last published 2025-10-22; Zed uses a pinned revision |
| macOS | Metal through wgpu | Metal, strong on Zed |
| Windows | winit, wgpu with DX12 and GL backends | Supported, shorter history |
| Linux | X11 and Wayland, Vulkan and GL | Vulkan and Wayland, heavier system inputs |
| Headless testing | `egui::Context::run` lays out and drives UI with no window; `egui_kittest` adds interaction tests | No equivalent public harness |
| Accessibility | AccessKit integration | Partial, tied to Zed |
| Release cadence | Regular 0.x releases with changelogs | Moves with the Zed repository |
| Repo history | None | The removed app used `gpui-kit` 0.6 and `gpui-wry`, with a 1024-package lockfile and a macOS-only webview |

The deciding rows are the published release and headless testing. The official `gpui` crate is a year stale, and the maintained path is a git revision with API churn. This repository's CI cannot open a window, so the UI needs a testable core and view logic. egui provides both. The visual polish of GPUI does not outweigh those costs for a device dashboard.

Pin `eframe` at the latest 0.37 patch that resolves, with default features plus `persistence`. `accesskit` and the wgpu backend are already default features in 0.37. Its declared MSRV is 1.95, below the crates' declared 1.98 and the stable toolchain in the dev shell. The design uses the core `eframe` and `egui` entry points: `eframe::run_native`, `eframe::App`, `egui::Context`, `Ui`, `CentralPanel`, `Frame` and `request_repaint_after`. It does not use `ViewportBuilder` or `Storage` directly; eframe persistence owns window geometry.

## Packaging and CI

### Nix

The GUI libraries go into `commonArgs` so the package and the checks share one dependency definition. The checks may pass without them because eframe, winit and wgpu load these libraries with `dlopen`, so the reason is the runtime closure, not clippy. The wrapper stays in the package, where it belongs.

```nix
commonArgs = {
  inherit src;
  strictDeps = true;
  nativeBuildInputs = [ pkgs.pkg-config ];
  buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [
    pkgs.wayland
    pkgs.libxkbcommon
    pkgs.xorg.libX11
    pkgs.xorg.libxcb
    pkgs.xorg.libXcursor
    pkgs.xorg.libXi
    pkgs.xorg.libXrandr
    pkgs.xorg.libXinerama
    pkgs.libGL
    pkgs.vulkan-loader
    pkgs.fontconfig
  ];
};
```

`echonet-radar/package.nix` builds the app with crane and wraps the binary on Linux. The wrapper sets `LD_LIBRARY_PATH` for those packages and `FONTCONFIG_FILE` to a `makeFontsConf` result, as the removed app did. It also installs the `.desktop` file, the icon, the permissions helper in the same `bin` directory, and the polkit policy. The helper shares the app's directory because it resolves the target application as its sibling.

`x86_64-darwin` stays out of scope for v1. Adding it requires an entry in `systems` and an override of the DeterminateCI runner map, because `macos-latest` is Apple Silicon. Intel macOS can be revisited with an Intel runner.

### macOS bundle

The package assembles `ECHONET Radar.app/Contents/MacOS/echonet-radar`, `Contents/Info.plist` and `Contents/Resources`. The plist carries:

```xml
<key>CFBundleIdentifier</key><string>dev.mokmok.echonet-radar</string>
<key>CFBundleName</key><string>ECHONET Radar</string>
<key>CFBundleExecutable</key><string>echonet-radar</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSLocalNetworkUsageDescription</key>
<string>ECHONET Radar finds ECHONET Lite appliances on your local network.</string>
<key>NSHighResolutionCapable</key><true/>
```

The Nix output is read-only, so the release job copies the `.app` to a writable temporary directory, signs the inner binary first and then the bundle with `codesign --force --options runtime --timestamp`, notarizes with `notarytool submit --wait`, staples, and zips. The workflow needs an Apple Developer ID and credentials, so it stays out of the default CI. A stable signing identity is a product requirement.

Apple's TN3179 states that `com.apple.developer.networking.multicast` is not required on macOS, so v1 does not ship an entitlements file. A sandboxed or App Store target would revisit that. Local Network permission tracks the code signature, so an unsigned or ad-hoc rebuild may be treated as a new app and prompt again. Application Firewall behavior for a rebuild is unverified.

Command-line tools launched from Terminal or SSH, including child processes, are automatically allowed local network access. A development build started with `cargo run` or `nix run` therefore does not exercise the Local Network prompt. The manual check launches the installed `.app` from Finder or `open`.

### Windows

`wix/main.wxs` targets WiX v4 and declares the rules at install time:

```xml
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs"
     xmlns:firewall="http://wixtoolset.org/schemas/v4/wxs/firewall">
  <Package Name="ECHONET Radar" Manufacturer="..." Version="0.1.0"
           UpgradeCode="..." Scope="perMachine">
    <ComponentGroup ...>
      <Component ...>
        <File Id="echonetRadarExe" Source="echonet-radar.exe" />
        <firewall:FirewallException Id="EchonetUdp3610Private"
            Name="EchonetRadarUdp3610Private"
            Program="[#echonetRadarExe]"
            Port="3610" Protocol="udp" Scope="localSubnet"
            Profile="private" IgnoreFailure="no" />
        <firewall:FirewallException Id="EchonetUdp3610Domain"
            Name="EchonetRadarUdp3610Domain"
            Program="[#echonetRadarExe]"
            Port="3610" Protocol="udp" Scope="localSubnet"
            Profile="domain" IgnoreFailure="no" />
      </Component>
    </ComponentGroup>
  </Package>
</Wix>
```

`Profile` takes one value, so the installer authors two rules. Both carry the app path, UDP 3610, local subnet scope and inbound allow. The MSI is per-machine and elevated, so the Windows Installer service can add the rules. `IgnoreFailure="no"` fails the install if a rule cannot be added, which is better than a silent first-run failure. Uninstall removes both rules. Portable builds under `%ProgramFiles%` use the helper; builds outside it use the manual command. One distribution channel per build, so the rules are not added twice.

The read-back looks for both names, checks every field, and reports `RulePresent` only when both match. A rule with the right display name but the wrong port, program, action or profile is `RuleAbsent`, not `RulePresent`.

Windows code signing is not available in v1. The MSI and the helper are unsigned, so SmartScreen warns and UAC shows an unknown publisher. The helper's trust check uses the install directory ACL. A certificate later can be added without changing the design.

The Windows CI job runs on `windows-latest` with a pinned toolchain (`dtolnay/rust-toolchain@1.99.0` and `cargo --locked`): `cargo build --workspace --release`, `cargo test --workspace`, and `cargo clippy --workspace --all-targets -- --deny warnings`. It builds the MSI with WiX, runs `msiexec /i ... /qn`, queries the two rules, uninstalls, and queries again. It also runs `echonet-radar --permission-inspect`, which must be parsed before `eframe` starts. That runner is an administrator, so it proves the read path only for an elevated account. A standard-user read, UAC, the Defender notification and real rule admission stay in the manual checklist.

### Linux

The Nix package is the primary distribution. It installs the app, the firewall helper in the same `bin` directory, `echonet-radar.desktop`, the icon, and the polkit action at `/usr/share/polkit-1/actions`. The polkit action uses `auth_admin`. AppImage is a later packaging step. Flatpak is out of scope for v1 because its network permission is all or nothing and it cannot add a host firewall rule. The UI reports `Unsupported` inside Flatpak.

### What CI cannot verify

- Any window on any platform. The Nix sandbox is headless and has no window server. macOS runners have no usable window server.
- The macOS Application Firewall and Local Network dialogs, and signed-identity behavior.
- Windows UAC, the Defender notification, and whether a rule actually admits traffic.
- polkit dialogs and whether a distribution firewall admits multicast.
- Real ECHONET multicast. The two existing UDP tests stay `#[ignore]`.
- Interactive MSI install and uninstall, and notarization.

These live behind a manual checklist and two seams: `Pulse` and `CommandRunner`. The pure layers carry the automated proof.

## Testing and verification

- `echonet-radar-core`: fold tests for `Device`, `Change`, `Seen` and `Status`, revision monotonicity, frame counters, and the class name on an object. A loopback session test starts, observes, stops and joins; it stays `#[ignore]` where the sandbox forbids multicast.
- `echonet-lite`: `class_name` lookup over every generated class, including a class absent from the appendix.
- `echonet-radar-permissions`: plan construction on every host with a fake `CommandRunner`; Windows JSON parsing with 0, 1 and 2 exit codes; firewalld and ufw backend detection that reports `NotObservable` instead of querying the rule; NixOS, Flatpak and Snap detection; helper argv golden tests, including a helper path with spaces and a single quote, and sibling target resolution; idempotent add and remove; rejected helper arguments.
- `echonet-radar`: `Context::run` tests that a snapshot renders the expected device, class name and value, and that a stale device is muted. `egui_kittest` interaction tests for selection, without image snapshots in CI. Verify the kittest harness in the sandbox before unit 6 depends on it; if it needs a rasterizer, keep CI on `Context::run` and move snapshots to a local-only test. Test the power chart through a pure `push_sample(series, snapshot, now)` function.
- Manual checklist on the macOS host. Launch the installed `.app` from Finder or `open`, not `cargo run`, because terminal-launched processes bypass the Local Network prompt. Check first run and each permission prompt, a manual rule edit in Firewall settings, discovery with a real appliance, interface change and restart, sleep and wake, and uninstall.
- The Windows and Linux manual checks are blocked until a host is available. When a host is available, they cover rule add and remove, UAC and polkit cancel, and the manual command. They are not a release gate for the macOS build, and they are a release gate for the Windows and Linux builds.

The current host has no display and no window server. This design cannot be runtime-verified here. The design review itself is a document check, not a product check.

`nix flake check --all-systems --no-build` evaluates only. CI and the release job run `nix build .#echonet-radar` per system, so the wrapper, the `.app` assembly and the package closure are built, not merely evaluated.

## Delivery plan

Each unit ends in a check. The first five units are GUI-independent and can land before a window exists.

| # | Unit | Files | Delivers | Check |
|---|---|---|---|---|
| 1 | `feat(echonet-lite): expose class names` | `echonet-lite-codegen/src/{ir,emit}.rs`, `echonet-lite/src/ecodec.rs`, `echonet-lite/src/ecodec/classes.rs` | `class_name` from `className.en` | lookup test over every generated class; clippy clean |
| 2 | `refactor(echonet-radar-core): split the service` | `core/src/{lib,protocol,format,model,service}.rs` | pure move, no behavior change | identical test count and results; diff is moves and re-exports |
| 3 | `feat(echonet-radar-core): build snapshots` | `model.rs`, `service.rs` | `RadarSnapshot`, `ObjectSnapshot`, `Observation`, `notices`, `frames_seen`, `last_frame`, `class_name` in the object | fold tests; revision monotonic; frame counters; class name present |
| 4 | `feat(echonet-radar-core): own the session` | `session.rs`, `lib.rs` | `RadarSession`, `RadarHandle`, `Pulse`, `Fault`, `SessionConfig`, `Command::{PollNow, Rediscover}`, `CommandError` | start, snapshot, stop joins; bind failure is `Fault`; rediscover sends a discovery frame; ignored loopback test |
| 5 | `refactor(echonet-radar-cli): consume the snapshot` | `cli/src/dashboard.rs`, `main.rs` | TUI dashboard reads `RadarSnapshot`; private fold deleted | CLI tests pass; net-negative diff |
| 6 | `feat(echonet-radar): add the read-only window` | new crate, `Cargo.toml`, `flake.nix`, `src/main.rs`, `src/app.rs`, `src/readiness.rs`, `settings.rs`, `net.rs`, `src/views/{status,start,devices,properties,settings}.rs` | window, start card, device list, read-only property pane, status bar, readiness state machine, interface picker, settings, restart | `Context::run` render test; every readiness row; settings round-trip and corrupt-file fallback; vanished interface falls back; `cargo build -p echonet-radar` |
| 7 | `feat(echonet-radar-permissions): inspect and plan` | new crate, `src/command.rs`, three platform modules | `Capability`, `Report`, `Plan`, `ApplyOutcome`, backend detection | fake runner tests; Windows JSON parsing with exit codes 0, 1 and 2; firewalld and ufw report `NotObservable`; argv golden tests on all hosts |
| 8 | `feat(echonet-radar-firewall-helper): elevate the rule` | new crate, Windows manifest, PowerShell scripts, polkit policy | add and remove the private and domain rules; trusted-prefix checks; `auth_admin` policy | rejected arguments; sibling target resolution; path with spaces and a quote; idempotent add and remove; manual UAC and polkit checks |
| 9 | `feat(echonet-radar): add the permission card` | `permission.rs`, `views/permission.rs`, `src/main.rs`, `views/settings.rs` | permission card, per-OS flow, `--permission-inspect`, and the permission entries in settings | one scenario per verdict; a rule never reaches `Listening`; `--permission-inspect` exits before eframe |
| 10 | `build: package the three OSes` | `package.nix`, `wix/main.wxs`, `Info.plist`, `packaging/linux/echonet-radar.desktop`, `packaging/macos/*`, `polkit/*`, `.github/workflows/{windows,release}.yaml` | Nix package with helper and policy, `.app`, unsigned MSI with two rules, Windows CI, release job | `nix build .#echonet-radar` per system; MSI silent install, rule query, uninstall; Windows job green; manual install |

Shared files that serialize work: `Cargo.toml`, `Cargo.lock`, `flake.nix`, `.github/workflows/*`, `echonet-radar-core/src/lib.rs` in unit 2, and `echonet-lite-codegen/src/emit.rs` in unit 1. Units 1 and 7 are disjoint. Units 3 and 4 touch the same core files and land in order. Unit 6 touches `Cargo.toml` and `flake.nix` and must not run alongside unit 10. Unit 9 edits `src/main.rs` and `views/settings.rs` after unit 6 creates them, so units 6 and 9 serialize on those two files.

### Platform spike before units 7 through 10

The permission and packaging units depend on platform behavior that CI cannot verify. macOS can run the spike. Windows and Linux hosts are not available, so their permission paths stay unverified until a host is available.

| Host | Status | Questions |
|---|---|---|
| macOS 15 or later, signed `.app` | Available | Does the Application Firewall prompt for a UDP bind with `SO_REUSEPORT`? Does Local Network prompt on multicast receive, and what happens on deny then retry? Does a Developer ID build without the multicast entitlement work? Does an ad-hoc rebuild re-prompt? |
| Windows 10 and 11, standard user and administrator | Blocked | Does a UDP bind trigger the Defender notification? Does `Get-NetFirewallRule` with the port and application filters read correctly as a standard user? Do the two MSI rules add on install and disappear on uninstall? Does the PowerShell helper handle a path with spaces and a single quote? |
| Linux with firewalld and with ufw | Blocked | Does `firewall-cmd --query-port` prompt or fail unprivileged? Does `--permanent --add-port` survive a reboot, and which zone receives it? Does `pkexec` run the Nix-store helper, and what does the polkit dialog show? Does ufw inactive accept a rule, and does it take effect when enabled? |

The design's platform claims are marked with the spike that confirms them. If a spike contradicts the matrix, update the matrix and the card copy before writing the helper.

The Windows and Linux permission cards can be implemented and unit-tested, but they are not release-ready until their spike runs. The macOS permission card can be verified in this cycle. The rest of the app is platform-neutral and testable on the macOS host.

### Throughput checkpoint

**Blocking first steps.** Units 1 through 4 define the API the app consumes. The permission crate's `Plan` contract in unit 7 blocks the permission card. The macOS spike gates unit 9 and the macOS part of unit 10; the Windows and Linux spikes are unavailable and gate only those releases. Units 1 through 6 proceed without any spike. Units 7 through 9 can be implemented, and their Windows and Linux paths stay unverified.

**Independent workstreams.** Protocol class names (unit 1), core model and session (units 3 and 4), permissions and helper (units 7 and 8), and packaging (unit 10) touch disjoint files. The read-only window (unit 6) can start against a stub snapshot as soon as unit 3 lands. The CLI migration (unit 5) depends on unit 3.

**Shared mutable state.** The workspace manifest and lockfile, the flake, the CI workflows, `core/src/lib.rs` during the split, and the codegen emitter. Serialize those. Every other file has one owner.

**Smallest safe decomposition.** One owner for the core model and session (units 3 and 4), because the snapshot and lifecycle invariants cross `model.rs`, `service.rs` and `session.rs`. Permissions can run in parallel. The GUI shell is a single owner after the snapshot type is stable.

## Tradeoffs accepted

- We accept rebuilding the snapshot at up to 10 Hz in exchange for a lock-free read and one writer.
- We accept snapshot rings of 500 changes and 20 notices in exchange for bounded memory and no unbounded event queue in the GUI.
- We accept no device eviction in core in exchange for keeping the service simple; the UI derives staleness.
- We accept a Linux port rule in the default zone in exchange for not shipping a privileged daemon.
- We accept that macOS, firewalld and ufw expose no readable rule state in exchange for no root helper on macOS and no auth prompt just to draw the card.
- We accept that in-app elevation is disabled outside a trusted install prefix, so development builds and portable builds outside `%ProgramFiles%` use manual steps.
- We accept a per-machine MSI and two firewall rules in exchange for a first launch that does not need UAC.
- We accept no raw nftables backend in exchange for not fighting distro-owned base chains.
- We accept a read-only v1 and defer writes to a later design, in exchange for a smaller surface and a tested permission flow first.
- We accept English-only UI in v1, with no localization layer and no CJK font.
- We accept wgpu's Linux runtime libraries in exchange for one backend across three OSes; a `glow` build stays possible if a target lacks Vulkan.
- We accept that the Windows and Linux permission paths are implemented but unverified until a host is available, in exchange for shipping the macOS path earlier.
- We accept that the CLI, including its dashboard mode, and the desktop app can race on 3610, documented, in exchange for not redesigning the CLI's socket ownership.

## Alternatives considered

**UI-side fold over the event stream.** The TUI's shape. It keeps core smaller, but it puts domain rules in the renderer, leaves the unbounded event queue in the GUI, and makes the UI responsible for reconstructing state. The snapshot keeps one writer and one decision point. Lost on interface depth and backpressure.

**Shared `Arc<Mutex<RadarModel>>`.** It gives the UI direct access, but a render that holds the lock blocks the receive loop, and two writers invite drift. A latest-wins immutable snapshot gives one source of truth with no read lock. Lost on concurrency safety.

**GPUI.** It renders better on macOS, but the published crate is a year stale, Windows and Linux support move with Zed, and there is no public headless harness. Lost on release stability and testability.

**A webview shell (Tauri or the removed `gpui-wry` app).** It buys a web UI at the cost of two dependency trees, webkit packaging, and a transport between the webview and the service. The removed app already paid that cost and was deleted. Lost on packaging and maintenance.

**A privileged daemon that owns the firewall.** It would make rules persistent and app-scoped on Linux via a custom nftables table, but it adds a long-running root service and an IPC surface for one port. Lost on attack surface and operational cost.

### Design provenance

This document synthesizes two independent candidate designs, two reviews, and the scope decisions from the operator. The base is the snapshot and session shape: one socket owner and immutable `Arc<RadarSnapshot>` values. From the second candidate it takes the settings and interface hardening, the `egui_kittest` test plan, and the consent-before-elevation rule. The architecture review fixed the start lifecycle contradiction, the readiness inputs and the missing settings unit. The platform review corrected the macOS entitlement claim, the firewalld read-back, the Windows filtered read-back, the WiX namespace and profiles, and the Nix runtime wrapper. The scope decisions removed the write path, the Set map, the control model and the encoder, and fixed English-only UI, arm64 macOS and the unsigned Windows build.

## Open questions and risks

1. macOS can be tested. Does a signed Developer ID build on macOS 15 or later show the Local Network prompt on multicast receive, and how do deny, allow and retry behave? Does the Application Firewall prompt fire for a UDP bind with `SO_REUSEPORT`? TN3179 says the multicast entitlement is not required. Run the macOS spike before unit 9.
2. Windows cannot be tested yet. Does a standard user get a working filtered read-back, when does the Defender notification appear for a UDP bind, and do the two MSI rules add and remove cleanly? Gate the Windows release on this spike.
3. Linux cannot be tested yet. Does `firewall-cmd --query-port` prompt or fail for a normal user, which zone receives `--add-port` without `--zone`, and does `pkexec` run the helper? Gate the Linux permission card and release on this spike.
4. Windows signing is unavailable. The MSI and helper ship unsigned, so SmartScreen warns and UAC shows an unknown publisher. The helper trusts the install directory ACL instead of a signature. A certificate later is a distribution improvement, not a design change.

## First implementation step

Add `class_name(class) -> &'static str` to `echonet-lite`, generated from `className.en`, with a test over every class in the MRA tables.

## References

- `echonet-radar-core/src/lib.rs`: `run_service`, `ServiceState`, `RadarEvent`, `Command`, `process_property_map`.
- `echonet-lite-udp/src/lib.rs`: `EchoNetSocket`, `bind_multicast`, `recv`, `send_frame_to`.
- `echonet-radar-cli/src/dashboard.rs`: the current fold and event loop.
- `echonet-lite/src/ecodec.rs`: `PropertyInfo`, `lookup`, `decode`.
- `echonet-lite-codegen/src/{ir,emit}.rs`: class metadata and generated tables.
- `flake.nix`, `.github/workflows/nix.yaml`, `echonet-radar-cli/package.nix`: build and CI constraints.
- Git commit `adbe2e4`: removal of the previous GPUI app.
- Microsoft Learn, "Configure Firewall Rules With Group Policy": inbound program and port rule behavior.
- Apple TN3179, "Local Network Privacy FAQ": macOS 15 local network access, `NSLocalNetworkUsageDescription`, and the statement that the multicast entitlement is not required on macOS.
- WiX v4 firewall extension: `FirewallException` namespace, attributes and profile values.
- firewalld, `firewall-cmd(1)` and the shipped polkit policy: `--permanent`, default zone, and `query-port` authorization.
- `pkexec(1)`: no path validation and no argument validation.
- Platform prompt behavior and the macOS Local Network permission come from platform documentation and general knowledge. They are marked for hardware verification, not asserted as tested here.
