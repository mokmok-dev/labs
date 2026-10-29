//! ECHONET Radar: a read-only window over the ECHONET Lite LAN.
//!
//! The window opens on a start card. Clicking **Start listening** binds UDP
//! 3610, sends one discovery request and shows the appliances that answer. The
//! service owns the socket; the window reads immutable snapshots and sends
//! commands only.
//!
//! ```text
//! echonet-radar                    # open the window and pick an interface
//! echonet-radar --interface br0    # use br0 for this run only
//! ```

mod app;
mod chart;
mod net;
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
use settings::InterfaceChoice;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(about = "Observe ECHONET Lite appliances in a window")]
struct Args {
    /// Interface to join the multicast group on, for this run only.
    #[arg(long, value_name = "NAME")]
    interface: Option<String>,
}

fn main() -> eframe::Result {
    let args = Args::parse();
    init_tracing();

    let (mut settings, notice) = settings::load();
    let stored_interface = settings.interface.clone();
    let interface_override = args.interface;
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
                interfaces,
                cc.egui_ctx.clone(),
            )))
        }),
    )
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
