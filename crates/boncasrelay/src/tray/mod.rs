#[cfg(any(target_os = "macos", windows))]
mod desktop;
#[cfg(not(any(target_os = "macos", windows)))]
mod linux;

#[cfg(any(target_os = "macos", windows))]
pub use desktop::run;
#[cfg(not(any(target_os = "macos", windows)))]
pub use linux::run;
