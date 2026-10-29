fn main() {
    #[cfg(windows)]
    {
        let embedded =
            embed_resource::compile("helper.rc", embed_resource::NONE).manifest_required();
        // Without the manifest the helper would start unelevated and fail on
        // every firewall call, so a missing resource must fail the build.
        if let Err(error) = embedded {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}
