//! The application's clock (restructure spec §4.1): ONE door to frame time. `now()` reads
//! `SDL_GetTicks` — the only call site of it in `app/` (the §15.2 gate: `SDL_GetTicks(` only in the
//! clock and the instruments). Main-thread only — the loop is the only reader, and a worker that
//! needs wall time uses `Instant` (an adapter).

#[cfg(not(test))]
extern "C" {
    fn SDL_GetTicks() -> u32;
}

/// The host test binary links no SDL (`diag::heartbeat` carries the same stub for the
/// performance counter): a test's clock is the value it set with [`set_for_test`], else zero.
#[cfg(test)]
#[allow(non_snake_case)]
unsafe fn SDL_GetTicks() -> u32 {
    TEST_NOW.with(|c| c.get())
}

#[cfg(test)]
thread_local! {
    static TEST_NOW: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Set this thread's clock, for a host test that grades a deadline.
#[cfg(test)]
pub(crate) fn set_for_test(ms: u32) {
    TEST_NOW.with(|c| c.set(ms));
}

/// Milliseconds since SDL init.
pub(crate) fn now() -> u32 {
    // SAFETY: SDL is initialised before the loop that reads the clock exists; the call takes no
    // arguments and touches no memory of ours.
    unsafe { SDL_GetTicks() }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_test_sets_the_clock_for_its_own_thread() {
        super::set_for_test(4242);
        assert_eq!(super::now(), 4242);
        super::set_for_test(0);
    }
}
