//! Code generator for the `echonet-lite` crate.
//!
//! Reads the pinned ECHONET Lite Machine Readable Appendix (MRA) under
//! `vendor/MRA_v1.4.0/` and emits deterministic Rust source into
//! `echonet-lite/src/ecodec/`.

mod emit;
mod ir;

use std::path::{Path, PathBuf};

use usage::Cli;

/// Generates Rust sources for the `echonet-lite` crate from the pinned ECHONET
/// Lite Machine Readable Appendix (MRA).
#[derive(Cli)]
#[usage(bin = "echonet-lite-codegen", version)]
struct CodegenArgs {
    /// Vendor specification directory to read from. Defaults to the
    /// workspace's `vendor/MRA_v1.4.0`.
    vendor: Option<PathBuf>,
    /// Output directory for generated codec sources. Defaults to
    /// `echonet-lite/src/ecodec` in the workspace.
    out: Option<PathBuf>,
}

fn main() -> Result<(), String> {
    let CodegenArgs { vendor, out } = CodegenArgs::parse();

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| String::from("codegen crate must be a workspace member"))?;
    let vendor = vendor.unwrap_or_else(|| workspace_root.join("vendor").join("MRA_v1.4.0"));
    let out = out.unwrap_or_else(|| {
        workspace_root
            .join("echonet-lite")
            .join("src")
            .join("ecodec")
    });

    let model = ir::Model::load(&vendor)?;
    emit::emit(&model, &out)?;

    println!(
        "generated {} classes, {} property entries into {out:?}",
        model.classes.len(),
        model.properties.values().map(Vec::len).sum::<usize>()
    );
    Ok(())
}
