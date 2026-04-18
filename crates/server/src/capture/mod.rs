//! Screen capture backends. `scrap` works on every supported OS;
//! `gdi` is the Windows lock-screen / Session-0 fallback used by the
//! agent process; `pipewire` is the Linux Wayland path (feature-gated).

pub mod scrap;

#[cfg(target_os = "windows")]
pub mod gdi;

#[cfg(feature = "wayland")]
pub mod pipewire;
