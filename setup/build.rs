// Builds the setup's window and gathers what it installs.
//
// The setup carries Platinum's three release programs and its icon inside itself
// (`include_bytes!` in src/payload.rs). They are copied here from `target/release` (or from
// `PLATINUM_PAYLOAD_DIR`) into OUT_DIR, and the build says whether they were found (`cfg(payload)`):
// a workspace build that has not built them yet still compiles, and such a setup refuses to
// install. scripts/build-setup.ps1 builds the programs first, then the setup, so the setup always
// carries the programs of the same build.
#[path = "../build_support/winres.rs"]
mod winres;

use std::path::PathBuf;

const PROGRAMS: [&str; 3] = ["platinum.exe", "platinum-start.exe", "platinum-ui.exe"];

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));

    // Texts are French in the .slint file (`@tr`), translated by lang/<code>/LC_MESSAGES/
    // platinum-setup.po, bundled into the program; the Rust side reads the same catalogs.
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent-dark".into())
        .with_bundled_translations(manifest.join("lang"))
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/setup.slint", config).expect("the setup's window does not compile");
    winres::embed_icon("../ui/platinum.ico", None);

    println!("cargo::rustc-check-cfg=cfg(payload)");
    println!("cargo:rerun-if-env-changed=PLATINUM_PAYLOAD_DIR");
    let source = std::env::var_os("PLATINUM_PAYLOAD_DIR").map(PathBuf::from).unwrap_or_else(|| manifest.join("../target/release"));
    let payload = out.join("payload");
    std::fs::create_dir_all(&payload).expect("cannot create the payload folder");
    let mut complete = true;
    for name in PROGRAMS {
        let from = source.join(name);
        println!("cargo:rerun-if-changed={}", from.display());
        let to = payload.join(name);
        if std::fs::copy(&from, &to).is_err() {
            complete = false;
            std::fs::write(&to, b"").expect("cannot write a payload placeholder");
        }
    }
    let icon = manifest.join("../ui/platinum.ico");
    std::fs::copy(&icon, payload.join("platinum.ico")).expect("cannot copy ui/platinum.ico");
    // Without the programs the setup still builds (a first `cargo build --workspace` builds the
    // programs and the setup side by side) and says at run time that it carries nothing.
    if complete {
        println!("cargo:rustc-cfg=payload");
    }
}
