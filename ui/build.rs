#[path = "../build_support/winres.rs"]
mod winres;

fn main() {
    // The window's texts are French in the .slint files (`@tr`), translated by the catalogs of
    // lang/<code>/LC_MESSAGES/platinum-ui.po, bundled into the program. Without a context, one
    // text has one translation wherever it is written, and the Rust side (platinum::i18n) reads the
    // same files by the same French keys.
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent-dark".into())
        .with_bundled_translations("lang")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app.slint", config).expect("the interface description does not compile");
    // The logo as platinum-ui.exe's own icon, so that the exe, its shortcut and its taskbar
    // button show it even when nothing else names an icon.
    winres::embed_icon("platinum.ico", None);
}
