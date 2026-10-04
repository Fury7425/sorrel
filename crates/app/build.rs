fn main() {
    // Icon resource 1 is what Explorer shows for the exe and what GPUI loads
    // as the window and taskbar icon.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("icon/sorrel.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
