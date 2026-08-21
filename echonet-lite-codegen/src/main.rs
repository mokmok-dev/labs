//! Code generator for the `echonet-lite` crate.
//!
//! Reads the pinned ECHONET Lite Machine Readable Appendix (MRA) under
//! `vendor/MRA_v1.4.0/` and emits deterministic Rust source into
//! `echonet-lite/src/ecodec/`.

mod emit;
mod ir;

use std::path::PathBuf;

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let vendor = args.next().map(PathBuf::from);
    let out = args.next().map(PathBuf::from);

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .expect("codegen crate must be a workspace member");

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
