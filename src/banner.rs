//! The `scala --version` banner string.

/// The Scala language level scalars targets — reported by `scala --version` so
/// build tools that parse the version line read a familiar level, followed by
/// the real engine so nothing is misrepresented as the JVM Scala.
pub const SCALA_COMPAT_VERSION: &str = "3.3";

/// The engine name — scalars is its own runtime (as the JVM is HotSpot).
pub const SCALA_ENGINE: &str = "scalars";

/// The `RUNTIME_PLATFORM` string, built from the host arch/OS.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The `scala --version` banner. Names the targeted language level, then the
/// real engine and its crate version and host triple.
pub fn version_banner() -> String {
    format!(
        "Scala code runner version {} (scalars {}) [{}]",
        SCALA_COMPAT_VERSION,
        env!("CARGO_PKG_VERSION"),
        platform()
    )
}
