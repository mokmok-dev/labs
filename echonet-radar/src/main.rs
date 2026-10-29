mod app;
mod chart;
mod net;
mod permission;
mod readiness;
mod settings;
#[cfg(test)]
mod test_support;
mod views;

use std::fs::{self, File, OpenOptions};
use std::sync::Mutex;

use clap::Parser;
use directories::ProjectDirs;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;

use app::{RadarApp, Startup};
use permission::PermissionController;
use settings::InterfaceChoice;

#[derive(Debug, Parser)]
#[command(about = "Observe ECHONET Lite appliances in a window")]
struct Args {
    /// Interface to join the multicast group on, for this run only.
    #[arg(long, value_name = "NAME")]
    interface: Option<String>,
    /// Print the firewall inspection report as JSON and exit.
    #[arg(long)]
    permission_inspect: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Launch {
    Inspect,
    Window { interface: Option<String> },
}

fn dispatch(args: Args) -> Launch {
    if args.permission_inspect {
        Launch::Inspect
    } else {
        Launch::Window {
            interface: args.interface,
        }
    }
}

fn main() -> eframe::Result {
    let launch = dispatch(Args::parse());
    let Launch::Window { interface } = launch else {
        print_inspection();
        return Ok(());
    };
    init_tracing();

    let (mut settings, notice) = settings::load();
    let stored_interface = settings.interface.clone();
    let interface_override = interface;
    if let Some(name) = &interface_override {
        settings.interface = InterfaceChoice::Named(name.clone());
    }
    let startup = Startup {
        settings,
        notice,
        path: settings::default_path(),
        stored_interface,
        interface_override,
    };
    let interfaces = net::enumerate();

    eframe::run_native(
        "ECHONET Radar",
        eframe::NativeOptions::default(),
        Box::new(move |cc| {
            Ok(Box::new(RadarApp::new(
                startup,
                PermissionController::start(),
                interfaces,
                cc.egui_ctx.clone(),
            )))
        }),
    )
}

fn print_inspection() {
    let report = echonet_radar_permissions::inspect(&permission::request());
    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("could not render the firewall report: {error}");
            std::process::exit(1);
        },
    }
}

fn init_tracing() {
    let writer: BoxMakeWriter = log_file().map_or_else(
        || BoxMakeWriter::new(std::io::stderr),
        |file| BoxMakeWriter::new(Mutex::new(file)),
    );
    let subscriber = tracing_subscriber::fmt().with_writer(writer).finish();
    // A host that already installed a subscriber must not stop the window.
    let _ = subscriber.try_init();
}

fn log_file() -> Option<File> {
    let directories = ProjectDirs::from("dev", "mokmok", "echonet-radar")?;
    let directory = directories.data_local_dir();
    fs::create_dir_all(directory).ok()?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("echonet-radar.log"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        Args::try_parse_from(args).expect("the arguments parse")
    }

    #[test]
    fn permission_inspect_returns_the_inspect_path() {
        let args = parse(&["echonet-radar", "--permission-inspect"]);
        assert!(args.permission_inspect);
        assert_eq!(dispatch(args), Launch::Inspect);
    }

    #[test]
    fn an_interface_override_cannot_pull_the_inspect_flag_into_the_window() {
        let args = parse(&[
            "echonet-radar",
            "--interface",
            "br0",
            "--permission-inspect",
        ]);
        assert_eq!(dispatch(args), Launch::Inspect);
    }

    #[test]
    fn a_plain_run_opens_the_window() {
        let args = parse(&["echonet-radar", "--interface", "br0"]);
        assert_eq!(
            dispatch(args),
            Launch::Window {
                interface: Some(String::from("br0"))
            }
        );
        assert_eq!(
            dispatch(parse(&["echonet-radar"])),
            Launch::Window { interface: None }
        );
    }
}
