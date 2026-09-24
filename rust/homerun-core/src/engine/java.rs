//! Java a host supplies to a descriptor game.
//!
//! A JVM server's descriptor names the Java major it needs
//! (`requires.java.major`) and `launch.program: "java"`; the host passes the
//! path of a runtime it already keeps, and the runner checks that runtime's
//! `java -version` before launching with it. This module holds the two
//! decisions both halves need to make identically: what a descriptor asks for,
//! and which major a `java -version` banner reports.
//!
//! # Why the runner checks at all
//!
//! The host chose the path, and the host is trusted to choose it. What the
//! host cannot promise is that the runtime there still works: a store can hold
//! a half-extracted or damaged JRE, and a server launched on the wrong major
//! fails in ways a player cannot read. Asking the program itself is the one
//! check that covers both.

use super::descriptor::{GameDescriptor, LaunchProgram};

/// The majors a descriptor may ask for.
///
/// 8 is the oldest Java any hostable server still targets; the ceiling only
/// exists so a typo (`250`) is refused rather than sent to a store that would
/// try to download it.
pub const MAJORS: std::ops::RangeInclusive<u32> = 8..=99;

/// The Java major this game needs from its host, if it needs one.
pub fn required_major(descriptor: &GameDescriptor) -> Option<u32> {
    descriptor.requires.java.map(|j| j.major)
}

/// Whether this platform's launch runs the host's Java rather than `exe`.
pub fn launches_host_java(descriptor: &GameDescriptor, host: &str) -> bool {
    descriptor
        .platform(host)
        .is_some_and(|p| p.launch.program == Some(LaunchProgram::Java))
}

/// The major version in a `java -version` banner.
///
/// Reads the first quoted version: `openjdk version "25.0.2" 2026-01-20` is
/// 25, `openjdk version "25" 2025-09-16` is 25, `java version "1.8.0_392"` is
/// 8 (the old `1.x` scheme), `openjdk version "21-ea"` is 21. `None` when no
/// version can be read, which the caller treats as a runtime that does not
/// work.
pub fn major_from_version(banner: &str) -> Option<u32> {
    let line = banner.lines().find(|l| l.contains("version \""))?;
    let start = line.find("version \"")? + "version \"".len();
    let rest = &line[start..];
    let version = &rest[..rest.find('"')?];
    let mut parts = version.split(|c: char| !c.is_ascii_digit());
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_major_is_read_from_each_banner_shape() {
        // Temurin 25, as Homerun Desktop bundles it.
        assert_eq!(
            major_from_version(
                "openjdk version \"25.0.2\" 2026-01-20 LTS\nOpenJDK Runtime Environment Temurin-25.0.2+10 (build 25.0.2+10-LTS)"
            ),
            Some(25)
        );
        assert_eq!(
            major_from_version("openjdk version \"25\" 2025-09-16"),
            Some(25)
        );
        assert_eq!(major_from_version("java version \"1.8.0_392\""), Some(8));
        assert_eq!(
            major_from_version("openjdk version \"21-ea\" 2023-09-19"),
            Some(21)
        );
        assert_eq!(
            major_from_version(
                "Picked up JAVA_TOOL_OPTIONS: -Xmx1g\nopenjdk version \"17.0.9\" 2023-10-17"
            ),
            Some(17)
        );
    }

    #[test]
    fn a_banner_without_a_version_is_not_a_java() {
        assert_eq!(major_from_version(""), None);
        assert_eq!(major_from_version("Error: could not find java.dll"), None);
        assert_eq!(major_from_version("openjdk version \"\""), None);
    }
}
