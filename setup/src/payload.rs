//! What the setup carries: Platinum's three release programs and its icon, built into the setup
//! by build.rs (`cfg(payload)` once the programs were found when the setup was built).

/// The files written into the install folder, by name.
#[cfg(payload)]
const FILES: &[(&str, &[u8])] = &[
    ("platinum.exe", include_bytes!(concat!(env!("OUT_DIR"), "/payload/platinum.exe"))),
    ("platinum-start.exe", include_bytes!(concat!(env!("OUT_DIR"), "/payload/platinum-start.exe"))),
    ("platinum-ui.exe", include_bytes!(concat!(env!("OUT_DIR"), "/payload/platinum-ui.exe"))),
    ("platinum.ico", include_bytes!(concat!(env!("OUT_DIR"), "/payload/platinum.ico"))),
];

#[cfg(not(payload))]
const FILES: &[(&str, &[u8])] = &[];

pub fn files() -> &'static [(&'static str, &'static [u8])] {
    FILES
}

/// Whether this setup was built with Platinum's programs in it.
pub fn present() -> bool {
    !FILES.is_empty()
}

/// Total size in bytes, for the "Installed apps" entry.
pub fn size() -> u64 {
    FILES.iter().map(|(_, bytes)| bytes.len() as u64).sum()
}
