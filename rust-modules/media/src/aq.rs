//! One-producer / one-consumer access-unit FIFO with byte-cap backpressure (was
//! src/aq.c). The player consumer allocates an `AuQueue` and passes `&q`; `aq_pop`
//! hands back a malloc'd `AuNode` the caller reads and frees. AuNode uses a flexible
//! array member (`data[]`); AuQueue embeds pthread objects — both repr(C) so the
//! layouts match glibc.
use std::os::raw::{c_int, c_long, c_uchar, c_void};
use std::ptr;

#[repr(C)]
pub struct AuNode {
    next: *mut AuNode,
    pts: i64,
    len: c_int,
    key: c_int,
    es: c_int,
    // flexible array `data[]` follows immediately after `es` (see node_data)
}

#[repr(C)]
pub struct AuQueue {
    head: *mut AuNode,
    tail: *mut AuNode,
    queued_bytes: c_long,
    max_bytes: c_long, // per-queue backpressure cap: `base_bytes` + any headroom granted
    base_bytes: c_long, // the cap the caller chose at aq_new; headroom is granted on top of it
    releasing: c_int, // headroom released but not yet drained: `aq_pop` walks `max_bytes` down
    pops: u64, // AUs the consumer has popped, ever: a producer's evidence that the lane is draining
    eof: c_int,
    abort: c_int,
    m: libc::pthread_mutex_t,
    not_full: libc::pthread_cond_t,
    not_empty: libc::pthread_cond_t,
}

/// pointer to the au_node's flexible `data[]` (offset = right after `es`)
#[inline]
unsafe fn node_data(n: *mut AuNode) -> *mut u8 {
    (n as *mut u8).add(core::mem::offset_of!(AuNode, es) + core::mem::size_of::<c_int>())
}

/// Crate-internal: has the consumer asked the producer to stop? (the demuxer checks this.)
///
/// `abort` is written under `m` by [`aq_abort`], so it must be read under the same mutex. The old
/// unlocked read was a Rust data race and, more practically, could miss teardown after its one
/// transport wake had already fired, letting the demuxer open a fresh connection under `join`.
#[inline]
pub unsafe fn aq_is_aborted(q: *const AuQueue) -> bool {
    if q.is_null() {
        return false;
    }
    let q = q as *mut AuQueue;
    libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
    let aborted = (*q).abort != 0;
    libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
    aborted
}

/// crate-internal: read a popped node's fields (es, key, pts, len, data ptr) —
/// the player engine's feed loop needs them without re-exposing the raw offsets.
#[inline]
pub unsafe fn au_fields(n: *mut AuNode) -> (c_int, c_int, i64, c_int, *const u8) {
    (
        (*n).es,
        (*n).key,
        (*n).pts,
        (*n).len,
        node_data(n) as *const u8,
    )
}

/// allocate + initialize a boxed queue with an explicit backpressure byte cap (mirrors
/// `stream::http_stream_boxed`; the two-lane feed gives each ES its own cap).
pub fn aq_new(cap: c_long) -> Box<AuQueue> {
    let mut q: Box<AuQueue> = Box::new(unsafe { std::mem::zeroed() });
    aq_init_cap(&mut *q, cap);
    q
}

/// initialize an already-allocated queue with an explicit backpressure byte cap.
fn aq_init_cap(q: *mut AuQueue, cap: c_long) {
    if q.is_null() {
        return;
    }
    unsafe {
        ptr::write_bytes(q as *mut u8, 0, core::mem::size_of::<AuQueue>());
        (*q).max_bytes = cap;
        (*q).base_bytes = cap;
        libc::pthread_mutex_init(ptr::addr_of_mut!((*q).m), ptr::null());
        init_not_full_cond(ptr::addr_of_mut!((*q).not_full));
        libc::pthread_cond_init(ptr::addr_of_mut!((*q).not_empty), ptr::null());
    }
}

/// `not_full` is the one condvar [`aq_park_deadline`] times out against — every other wait in
/// this file is untimed. On Linux, bind it to `CLOCK_MONOTONIC` so that timeout tracks elapsed
/// time even if the wall clock steps (NTP correction; this TV's own pmlog clock is documented as
/// running skewed) — a REALTIME-based timed wait can block far past its 10 ms deadline on a
/// backward step, which is exactly the TCP-window-collapse this drain-while-parked mechanism
/// exists to prevent. `pthread_condattr_setclock` does not exist on macOS, so the host build (and
/// its tests, which check the drain LOGIC, not clock robustness) keeps the default REALTIME
/// clock — matched by [`aq_park_deadline`]'s `#[cfg]` twin.
#[cfg(target_os = "linux")]
unsafe fn init_not_full_cond(cond: *mut libc::pthread_cond_t) {
    unsafe {
        let mut attr: libc::pthread_condattr_t = std::mem::zeroed();
        libc::pthread_condattr_init(&mut attr);
        libc::pthread_condattr_setclock(&mut attr, libc::CLOCK_MONOTONIC);
        libc::pthread_cond_init(cond, &attr);
        libc::pthread_condattr_destroy(&mut attr);
    }
}

#[cfg(not(target_os = "linux"))]
unsafe fn init_not_full_cond(cond: *mut libc::pthread_cond_t) {
    unsafe {
        libc::pthread_cond_init(cond, ptr::null());
    }
}

pub fn aq_destroy(q: *mut AuQueue) {
    if q.is_null() {
        return;
    }
    unsafe {
        libc::pthread_mutex_destroy(ptr::addr_of_mut!((*q).m));
        libc::pthread_cond_destroy(ptr::addr_of_mut!((*q).not_full));
        libc::pthread_cond_destroy(ptr::addr_of_mut!((*q).not_empty));
    }
}

/// [`aq_push`] that runs `drain` whenever the queue is full.
///
/// Original playback parks on this path on the same thread as AVIO. The drain keeps the media
/// socket's TCP window from collapsing to zero while the consumer holds the cap.
///
/// `drain` returns whether another short poll is useful: `true` means bounce still has room so
/// the peer may still fill; `false` means cap/abort/I/O failure, so this waits on `not_full`
/// instead of waking every 10 ms.
pub fn aq_push_with_drain(
    q: *mut AuQueue,
    data: *const c_uchar,
    len: c_int,
    pts: i64,
    key: c_int,
    es: c_int,
    mut drain: impl FnMut() -> bool,
) -> c_int {
    aq_push_park(q, data, len, pts, key, es, Some(&mut drain))
}

/// A `not_full` timed-wait deadline 10 ms out, on the SAME clock `init_not_full_cond` bound the
/// condvar to — Linux gets `CLOCK_MONOTONIC` (immune to the wall-clock step this device is
/// documented to experience), everywhere else keeps the wall clock the condvar itself defaults
/// to. Mixing clock bases between the condvar and its deadline would make the timeout meaningless
/// rather than merely imprecise.
#[cfg(target_os = "linux")]
fn aq_park_deadline() -> libc::timespec {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    add_10ms(ts)
}

#[cfg(not(target_os = "linux"))]
fn aq_park_deadline() -> libc::timespec {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    unsafe {
        libc::gettimeofday(&mut tv, ptr::null_mut());
    }
    add_10ms(libc::timespec {
        tv_sec: tv.tv_sec,
        tv_nsec: i64::from(tv.tv_usec) * 1000,
    })
}

fn add_10ms(ts: libc::timespec) -> libc::timespec {
    let mut nsec = ts.tv_nsec + 10_000_000;
    let mut sec = ts.tv_sec;
    if nsec >= 1_000_000_000 {
        sec += 1;
        nsec -= 1_000_000_000;
    }
    libc::timespec {
        tv_sec: sec,
        tv_nsec: nsec as libc::c_long,
    }
}

fn aq_push_park(
    q: *mut AuQueue,
    data: *const c_uchar,
    len: c_int,
    pts: i64,
    key: c_int,
    es: c_int,
    mut drain: Option<&mut dyn FnMut() -> bool>,
) -> c_int {
    if q.is_null() || len < 0 {
        return -1;
    }
    unsafe {
        let n = libc::malloc(core::mem::size_of::<AuNode>() + len as usize) as *mut AuNode;
        if n.is_null() {
            return -1;
        }
        (*n).next = ptr::null_mut();
        (*n).pts = pts;
        (*n).len = len;
        (*n).key = key;
        (*n).es = es;
        if len > 0 && !data.is_null() {
            ptr::copy_nonoverlapping(data, node_data(n), len as usize);
        }
        libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
        while (*q).queued_bytes > (*q).max_bytes && (*q).abort == 0 {
            if let Some(drain) = drain.as_mut() {
                libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
                let keep_polling = drain();
                libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
                if (*q).queued_bytes > (*q).max_bytes && (*q).abort == 0 {
                    if keep_polling {
                        let ts = aq_park_deadline();
                        libc::pthread_cond_timedwait(
                            ptr::addr_of_mut!((*q).not_full),
                            ptr::addr_of_mut!((*q).m),
                            &ts,
                        );
                    } else {
                        libc::pthread_cond_wait(
                            ptr::addr_of_mut!((*q).not_full),
                            ptr::addr_of_mut!((*q).m),
                        );
                    }
                }
            } else {
                libc::pthread_cond_wait(
                    ptr::addr_of_mut!((*q).not_full),
                    ptr::addr_of_mut!((*q).m),
                );
            }
        }
        if (*q).abort != 0 {
            libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
            libc::free(n as *mut c_void);
            return -1;
        }
        if !(*q).tail.is_null() {
            (*(*q).tail).next = n;
        } else {
            (*q).head = n;
        }
        (*q).tail = n;
        (*q).queued_bytes += len as c_long;
        libc::pthread_cond_signal(ptr::addr_of_mut!((*q).not_empty));
        libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
        0
    }
}

/// Consumer: pop the next AU (caller frees it) or NULL. Never blocks.
/// *eof_out = 1 when the producer finished and the queue is drained.
pub fn aq_pop(q: *mut AuQueue, eof_out: *mut c_int) -> *mut AuNode {
    if q.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
        let n = (*q).head;
        if !n.is_null() {
            (*q).head = (*n).next;
            if (*q).head.is_null() {
                (*q).tail = ptr::null_mut();
            }
            (*q).queued_bytes -= (*n).len as c_long;
            (*q).pops += 1;
            if (*q).releasing != 0 {
                settle_released_cap(q);
            }
            libc::pthread_cond_signal(ptr::addr_of_mut!((*q).not_full));
        }
        if !eof_out.is_null() {
            *eof_out = if (*q).head.is_null() && (*q).eof != 0 {
                1
            } else {
                0
            };
        }
        libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
        n
    }
}

pub fn aq_set_eof(q: *mut AuQueue) {
    if q.is_null() {
        return;
    }
    unsafe {
        libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
        (*q).eof = 1;
        libc::pthread_cond_signal(ptr::addr_of_mut!((*q).not_empty));
        libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
    }
}

pub fn aq_abort(q: *mut AuQueue) {
    if q.is_null() {
        return;
    }
    unsafe {
        libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
        (*q).abort = 1;
        libc::pthread_cond_broadcast(ptr::addr_of_mut!((*q).not_full));
        libc::pthread_cond_broadcast(ptr::addr_of_mut!((*q).not_empty));
        libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
    }
}

/// Producer: let this queue hold `extra` bytes beyond the cap it was created with, until the next
/// call. Raising the cap wakes a producer parked on `not_full`, so the grant takes effect even for a
/// push already waiting. Granting (re)starts a headroom: it cancels a pending
/// [`aq_release_headroom`], which is how a seek or reopen begins again.
///
/// `extra == 0` snaps the cap straight back to the base, which can leave it BELOW what the queue
/// holds: the next push then parks until the consumer has drained under it. That is right for a
/// queue nobody was relying on the headroom for; a producer that needs the other lane to keep
/// being read while this one is over its base cap wants [`aq_release_headroom`] instead.
pub fn aq_set_headroom(q: *mut AuQueue, extra: c_long) {
    if q.is_null() {
        return;
    }
    unsafe {
        libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
        let prev = (*q).max_bytes;
        (*q).max_bytes = (*q).base_bytes + extra.max(0);
        (*q).releasing = 0;
        if (*q).max_bytes > prev {
            libc::pthread_cond_broadcast(ptr::addr_of_mut!((*q).not_full));
        }
        libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
    }
}

/// Producer: take the headroom away **as the consumer drains the queue, never below what it holds**.
///
/// The cap becomes the queue's current occupancy (at least the base) and then follows `aq_pop`
/// downward until it is the base again, after which this is a plain queue. Contrast
/// `aq_set_headroom(q, 0)`, which drops the cap to the base at once: with the queue already past
/// the base that parks the producer on its very next push, and a demuxer that feeds two lanes
/// from one sequential read then cannot read the OTHER lane's next packets until this lane has
/// drained, which it cannot do before the packets it is blocking arrive. Here the producer may
/// still finish the push it is on (occupancy is not over the cap), and parks from the one after
/// until the consumer pops — the same lock-step a queue that sits AT its cap always shows.
/// The cap only ever falls here, so nobody needs waking: a producer already parked stays parked until a pop
/// takes the queue under the cap it parked on.
///
/// A queue at or under its base cap is simply restored to the base. A later
/// `aq_set_headroom(q, extra)` (a seek or reopen) cancels the release and grants afresh.
pub fn aq_release_headroom(q: *mut AuQueue) {
    if q.is_null() {
        return;
    }
    unsafe {
        libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
        (*q).releasing = 1;
        settle_released_cap(q);
        libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
    }
}

/// With `m` held and a release pending: pull the cap down to the occupancy (never under the base,
/// never up), and end the release once it has reached the base.
unsafe fn settle_released_cap(q: *mut AuQueue) {
    unsafe {
        let floor = (*q).base_bytes;
        (*q).max_bytes = (*q).max_bytes.min((*q).queued_bytes).max(floor);
        if (*q).max_bytes <= floor {
            (*q).releasing = 0;
        }
    }
}

/// The cap in force right now: the base, plus any headroom granted and not yet drained away.
pub fn aq_cap(q: *mut AuQueue) -> c_long {
    if q.is_null() {
        return 0;
    }
    unsafe {
        libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
        let cap = (*q).max_bytes;
        libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
        cap
    }
}

/// Would a push made now park? Producer-side, advisory: the consumer can only make the answer
/// "no" (a pop) between this call and the push. Same predicate `aq_push` waits on.
pub fn aq_would_park(q: *mut AuQueue) -> bool {
    if q.is_null() {
        return false;
    }
    unsafe {
        libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
        let parks = (*q).queued_bytes > (*q).max_bytes && (*q).abort == 0;
        libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
        parks
    }
}

/// How many AUs the consumer has popped from this queue since it was created. Monotonic, so a
/// producer that records it at one moment and reads it at a later one learns whether the consumer
/// popped anything in between: the difference between a lane that is full because nobody drains it
/// and one that is full because the producer is faster than the drain.
pub fn aq_pops(q: *mut AuQueue) -> u64 {
    if q.is_null() {
        return 0;
    }
    unsafe {
        libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
        let n = (*q).pops;
        libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
        n
    }
}

pub fn aq_bytes(q: *mut AuQueue) -> c_long {
    if q.is_null() {
        return 0;
    }
    unsafe {
        libc::pthread_mutex_lock(ptr::addr_of_mut!((*q).m));
        let b = (*q).queued_bytes;
        libc::pthread_mutex_unlock(ptr::addr_of_mut!((*q).m));
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn pop_one(q: *mut AuQueue) {
        let n = aq_pop(q, ptr::null_mut());
        assert!(!n.is_null(), "the queue had an AU to pop");
        unsafe { libc::free(n as *mut c_void) };
    }

}
