fn main() {
    #[cfg(windows)]
    {
        embed_resource::compile("helper.manifest", embed_resource::NONE);
    }
}
