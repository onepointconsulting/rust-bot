//! Named points where tests can simulate a process kill.
//!
//! [`crash_point`] is a no-op in normal builds. In unit tests a point armed
//! with [`arm`] panics when it is reached (the `nth` time), so the surrounding
//! turn dies exactly where a `kill -9` would leave it: nothing after the point
//! runs and, because a panicking thread writes no journal terminals (see
//! `agent::turn_journal`), the journal looks like a crashed process wrote it.
//!
//! Armed state is per thread, so parallel tests cannot trip each other.

#[cfg(test)]
mod armed {
    use std::cell::RefCell;

    pub struct Armed {
        pub name: &'static str,
        pub skip: usize,
    }

    thread_local! {
        pub static ARMED: RefCell<Option<Armed>> = const { RefCell::new(None) };
    }
}

/// Crash the current thread the `nth` time (0 = first) the point `name` is reached.
#[cfg(test)]
pub fn arm(name: &'static str, nth: usize) {
    armed::ARMED.with(|armed| {
        *armed.borrow_mut() = Some(armed::Armed { name, skip: nth });
    });
}

/// Disarm any armed crash point of the current thread.
#[cfg(test)]
pub fn disarm() {
    armed::ARMED.with(|armed| *armed.borrow_mut() = None);
}

/// Mark a place where a kill could happen. Free in release builds.
#[inline]
pub fn crash_point(name: &'static str) {
    #[cfg(test)]
    {
        let fire = armed::ARMED.with(|armed| {
            let mut armed = armed.borrow_mut();
            let Some(point) = armed.as_mut() else {
                return false;
            };
            if point.name != name {
                return false;
            }
            if point.skip > 0 {
                point.skip -= 1;
                return false;
            }
            *armed = None;
            true
        });
        if fire {
            panic!("simulated crash at {name}");
        }
    }
    #[cfg(not(test))]
    let _ = name;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::catch_unwind;

    #[test]
    fn an_unarmed_point_does_nothing() {
        crash_point("anywhere");
    }

    #[test]
    fn an_armed_point_panics_once_and_then_disarms() {
        arm("here", 0);
        crash_point("elsewhere");
        assert!(catch_unwind(|| crash_point("here")).is_err());
        crash_point("here");
    }

    #[test]
    fn nth_skips_earlier_hits() {
        arm("here", 2);
        crash_point("here");
        crash_point("here");
        assert!(catch_unwind(|| crash_point("here")).is_err());
        disarm();
    }
}
