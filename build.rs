// Gives platinum.exe and platinum-start.exe the logo as their icon (Explorer, Task Manager, and
// any shortcut that does not name an icon of its own). No resource compiler is needed: the module
// writes the resource object itself (see build_support/winres.rs).
#[path = "build_support/winres.rs"]
mod winres;

fn main() {
    winres::embed_icon("ui/platinum.ico", None);
}
