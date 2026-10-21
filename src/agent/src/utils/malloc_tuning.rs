//! Default glibc allocator tuning for long-running deployments.
//!
//! rust-bot's workload is bursty (images, article HTML, LLM payloads). With
//! glibc's defaults each tokio worker gets its own malloc arena and freed
//! memory is rarely trimmed, so RSS climbs to the busiest moment's peak and
//! stays there. Two tunables fix most of that:
//!
//! * `MALLOC_ARENA_MAX` — caps the number of arenas so freed memory is reused
//!   across threads instead of being parked in many partly empty arenas.
//! * `MALLOC_TRIM_THRESHOLD_` — returns free top-of-heap memory to the OS
//!   sooner. Setting it also turns off glibc's *dynamic* mmap threshold, which
//!   otherwise grows after each large free and pushes later large buffers
//!   (such as image payloads) into arenas instead of freeing them via `munmap`.
//!
//! glibc reads those environment variables only once, before `main`, so
//! setting them from inside the process is too late. [`apply_defaults`] uses
//! `mallopt(3)` instead, and is meant to be called from a constructor that runs
//! before the async runtime spawns threads. A variable already present in the
//! environment always wins, so a deployment can still override or tune these.
//!
//! Only Linux + glibc is affected; everywhere else [`apply_defaults`] is a no-op.

/// A glibc malloc tunable that rust-bot defaults.
#[derive(Debug, PartialEq, Eq)]
pub struct MallocTunable {
    /// Environment variable that, when set, overrides [`Self::default_value`].
    pub env_name: &'static str,
    /// `mallopt(3)` parameter id (the `M_*` constant from `<malloc.h>`).
    pub mallopt_param: i32,
    /// Value applied when the environment variable is not set.
    pub default_value: i32,
}

/// Maximum number of malloc arenas (`M_ARENA_MAX` is `-8` in `<malloc.h>`).
pub const ARENA_MAX_TUNABLE: MallocTunable = MallocTunable {
    env_name: "MALLOC_ARENA_MAX",
    mallopt_param: -8,
    default_value: 2,
};

/// Free top-of-heap size, in bytes, that triggers a trim (`M_TRIM_THRESHOLD`
/// is `-1` in `<malloc.h>`). 128 KiB is glibc's own fixed default before it
/// starts adapting.
pub const TRIM_THRESHOLD_TUNABLE: MallocTunable = MallocTunable {
    env_name: "MALLOC_TRIM_THRESHOLD_",
    mallopt_param: -1,
    default_value: 131_072,
};

/// Every tunable rust-bot defaults, in application order.
pub const DEFAULT_TUNABLES: [&MallocTunable; 2] = [&ARENA_MAX_TUNABLE, &TRIM_THRESHOLD_TUNABLE];

/// The tunables that should be applied: those whose environment variable is
/// not set. `is_env_set` is injected so the decision is testable without
/// touching the real process environment.
pub fn tunables_to_apply(is_env_set: impl Fn(&str) -> bool) -> Vec<&'static MallocTunable> {
    DEFAULT_TUNABLES
        .iter()
        .copied()
        .filter(|tunable| !is_env_set(tunable.env_name))
        .collect()
}

/// Apply the default allocator tuning, skipping any tunable the environment
/// already sets. Call once, as early as possible (before spawning threads).
/// No-op unless the target is Linux with glibc.
pub fn apply_defaults() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    for tunable in tunables_to_apply(|name| std::env::var_os(name).is_some()) {
        // SAFETY: `mallopt` only adjusts allocator parameters and is safe to
        // call at any time; a failure (return 0) just leaves glibc's default.
        let applied = unsafe { libc::mallopt(tunable.mallopt_param, tunable.default_value) };
        if applied == 0 {
            eprintln!(
                "warning: mallopt could not set {} to {}",
                tunable.env_name, tunable.default_value
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_every_default_when_environment_is_empty() {
        let applied = tunables_to_apply(|_| false);
        assert_eq!(applied, vec![&ARENA_MAX_TUNABLE, &TRIM_THRESHOLD_TUNABLE]);
    }

    #[test]
    fn skips_tunable_already_set_in_environment() {
        let applied = tunables_to_apply(|name| name == "MALLOC_ARENA_MAX");
        assert_eq!(applied, vec![&TRIM_THRESHOLD_TUNABLE]);
    }

    #[test]
    fn applies_nothing_when_environment_sets_everything() {
        assert!(tunables_to_apply(|_| true).is_empty());
    }

    #[test]
    fn defaults_match_documented_values() {
        assert_eq!(ARENA_MAX_TUNABLE.default_value, 2);
        assert_eq!(TRIM_THRESHOLD_TUNABLE.default_value, 131_072);
    }

    /// `mallopt` must accept the defaults on glibc without crashing.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn apply_defaults_runs_on_glibc() {
        apply_defaults();
    }
}
