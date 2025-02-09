use crate::io::interest::Interest;
use crate::io::ready::Ready;
use crate::loom::sync::atomic::AtomicUsize;
use crate::loom::sync::Mutex;
use crate::runtime::io::{Direction, ReadyEvent, Tick};
use crate::util::bit;
use crate::util::linked_list::{self, LinkedList};
use crate::util::WakeList;

use std::cell::UnsafeCell;
use std::future::Future;
use std::marker::PhantomPinned;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::{AcqRel, Acquire};
use std::task::{Context, Poll, Waker};

/// Stored in the I/O driver resource slab.
#[derive(Debug)]
pub(crate) struct ScheduledIo {
    pub(super) linked_list_pointers: UnsafeCell<linked_list::Pointers<Self>>,

    /// Packs the resource's readiness and I/O driver latest tick.
    readiness: AtomicUsize,

    waiters: Mutex<Waiters>,
}

type WaitList = LinkedList<Waiter, <Waiter as linked_list::Link>::Target>;

#[derive(Debug, Default)]
struct Waiters {
    /// List of all current waiters.
    list: WaitList,

    /// Waker used for AsyncRead.
    reader: Option<Waker>,

    /// Waker used for AsyncWrite.
    writer: Option<Waker>,
}

#[derive(Debug)]
struct Waiter {
    pointers: linked_list::Pointers<Waiter>,

    /// The waker for this task.
    waker: Option<Waker>,

    /// The interest this waiter is waiting on.
    interest: Interest,

    is_ready: bool,

    /// Should never be `!Unpin`.
    _p: PhantomPinned,
}

generate_addr_of_methods! {
    impl<> Waiter {
        unsafe fn addr_of_pointers(self: NonNull<Self>) -> NonNull<linked_list::Pointers<Waiter>> {
            &self.pointers
        }
    }
}

/// Future returned by `readiness()`.
struct Readiness<'a> {
    scheduled_io: &'a ScheduledIo,

    state: State,

    /// Entry in the waiter `LinkedList`.
    waiter: UnsafeCell<Waiter>,
}

enum State {
    Init,
    Waiting,
    Done,
}

// The `ScheduledIo::readiness` (`AtomicUsize`) is packed full of goodness.
//
// | shutdown | driver tick | readiness |
// |----------+-------------+-----------|
// |   1 bit  |   8 bits    +   16 bits |

const READINESS: bit::Pack = bit::Pack::least_significant(16);

const TICK: bit::Pack = READINESS.then(8);

const SHUTDOWN: bit::Pack = TICK.then(1);

// ===== impl ScheduledIo =====

impl Default for ScheduledIo {
    fn default() -> ScheduledIo {
        ScheduledIo {
            linked_list_pointers: UnsafeCell::new(linked_list::Pointers::new()),
            readiness: AtomicUsize::new(0),
            waiters: Mutex::new(Default::default()),
        }
    }
}

impl ScheduledIo {
    pub(crate) fn token(&self) -> mio::Token {
        // use `expose_addr` when stable
        mio::Token(self as *const _ as usize)
    }

    /// Invoked when the IO driver is shut down; forces this ScheduledIo into a
    /// permanently shutdown state.
    pub(super) fn shutdown(&self) {
        let mask = SHUTDOWN.pack(1, 0);
        self.readiness.fetch_or(mask, AcqRel);
        self.wake(Ready::ALL);
    }

    /// Sets the readiness on this `ScheduledIo` by invoking the given closure on
    /// the current value, returning the previous readiness value.
    ///
    /// # Arguments
    /// - `tick`: whether setting the tick or trying to clear readiness for a
    ///    specific tick.
    /// - `f`: a closure returning a new readiness value given the previous
    ///   readiness.
    pub(super) fn set_readiness(&self, tick: Tick, f: impl Fn(Ready) -> Ready) {
        let mut current = self.readiness.load(Acquire);

        // The shutdown bit should not be set
        debug_assert_eq!(0, SHUTDOWN.unpack(current));

        loop {
            // Mask out the tick bits so that the modifying function doesn't see
            // them.
            let current_readiness = Ready::from_usize(current);
            let new = f(current_readiness);

            let next = match tick {
                Tick::Set(t) => TICK.pack(t as usize, new.as_usize()),
                Tick::Clear(t) => {
                    if TICK.unpack(current) as u8 != t {
                        // Trying to clear readiness with an old event!
                        return;
                    }

                    TICK.pack(t as usize, new.as_usize())
                }
            };

            match self
                .readiness
                .compare_exchange(current, next, AcqRel, Acquire)
            {
                Ok(_) => return,
                // we lost the race, retry!
                Err(actual) => current = actual,
            }
        }
    }

    /// Notifies all pending waiters that have registered interest in `ready`.
    ///
    /// There may be many waiters to notify. Waking the pending task **must** be
    /// done from outside of the lock otherwise there is a potential for a
    /// deadlock.
    ///
    /// A stack array of wakers is created and filled with wakers to notify, the
    /// lock is released, and the wakers are notified. Because there may be more
    /// than 32 wakers to notify, if the stack array fills up, the lock is
    /// released, the array is cleared, and the iteration continues.
    pub(super) fn wake(&self, ready: Ready) {
        let mut wakers = WakeList::new();

        let mut waiters = self.waiters.lock();

        // check for AsyncRead slot
        if ready.is_readable() {
            if let Some(waker) = waiters.reader.take() {
                wakers.push(waker);
            }
        }

        // check for AsyncWrite slot
        if ready.is_writable() {
            if let Some(waker) = waiters.writer.take() {
                wakers.push(waker);
            }
        }

        'outer: loop {
            let mut iter = waiters.list.drain_filter(|w| ready.satisfies(w.interest));

            while wakers.can_push() {
                match iter.next() {
                    Some(waiter) => {
                        let waiter = unsafe { &mut *waiter.as_ptr() };

                        if let Some(waker) = waiter.waker.take() {
                            waiter.is_ready = true;
                            wakers.push(waker);
                        }
                    }
                    None => {
                        break 'outer;
                    }
                }
            }

            drop(waiters);

            wakers.wake_all();

            // Acquire the lock again.
            waiters = self.waiters.lock();
        }

        // Release the lock before notifying
        drop(waiters);

        wakers.wake_all();
    }

    pub(super) fn ready_event(&self, interest: Interest) -> ReadyEvent {
        let curr = self.readiness.load(Acquire);

        ReadyEvent {
            tick: TICK.unpack(curr) as u8,
            ready: interest.mask() & Ready::from_usize(READINESS.unpack(curr)),
            is_shutdown: SHUTDOWN.unpack(curr) != 0,
        }
    }

    /// Polls for readiness events in a given direction.
    ///
    /// These are to support `AsyncRead` and `AsyncWrite` polling methods,
    /// which cannot use the `async fn` version. This uses reserved reader
    /// and writer slots.
    pub(super) fn poll_readiness(
        &self,
        cx: &mut Context<'_>,
        direction: Direction,
    ) -> Poll<ReadyEvent> {
        let curr = self.readiness.load(Acquire);

        let ready = direction.mask() & Ready::from_usize(READINESS.unpack(curr));
        let is_shutdown = SHUTDOWN.unpack(curr) != 0;

        if ready.is_empty() && !is_shutdown {
            // Update the task info
            let mut waiters = self.waiters.lock();
            let slot = match direction {
                Direction::Read => &mut waiters.reader,
                Direction::Write => &mut waiters.writer,
            };

            // Avoid cloning the waker if one is already stored that matches the
            // current task.
            match slot {
                Some(existing) => {
                    if !existing.will_wake(cx.waker()) {
                        *existing = cx.waker().clone();
                    }
                }
                None => {
                    *slot = Some(cx.waker().clone());
                }
            }

            // Try again, in case the readiness was changed while we were
            // taking the waiters lock
            let curr = self.readiness.load(Acquire);
            let ready = direction.mask() & Ready::from_usize(READINESS.unpack(curr));
            let is_shutdown = SHUTDOWN.unpack(curr) != 0;
            if is_shutdown {
                Poll::Ready(ReadyEvent {
                    tick: TICK.unpack(curr) as u8,
                    ready: direction.mask(),
                    is_shutdown,
                })
            } else if ready.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(ReadyEvent {
                    tick: TICK.unpack(curr) as u8,
                    ready,
                    is_shutdown,
                })
            }
        } else {
            Poll::Ready(ReadyEvent {
                tick: TICK.unpack(curr) as u8,
                ready,
                is_shutdown,
            })
        }
    }

    pub(crate) fn clear_readiness(&self, event: ReadyEvent) {
        // This consumes the current readiness state **except** for closed
        // states. Closed states are excluded because they are final states.
        let mask_no_closed = event.ready - Ready::READ_CLOSED - Ready::WRITE_CLOSED;
        self.set_readiness(Tick::Clear(event.tick), |curr| curr - mask_no_closed);
    }

    pub(crate) fn clear_wakers(&self) {
        let mut waiters = self.waiters.lock();
        waiters.reader.take();
        waiters.writer.take();
    }
}

impl Drop for ScheduledIo {
    fn drop(&mut self) {
        self.wake(Ready::ALL);
    }
}

unsafe impl Send for ScheduledIo {}
unsafe impl Sync for ScheduledIo {}

impl ScheduledIo {
    /// An async version of `poll_readiness` which uses a linked list of wakers.
    pub(crate) async fn readiness(&self, interest: Interest) -> ReadyEvent {
        self.readiness_fut(interest).await
    }

    // This is in a separate function so that the borrow checker doesn't think
    // we are borrowing the `UnsafeCell` possibly over await boundaries.
    //
    // Go figure.
    fn readiness_fut(&self, interest: Interest) -> Readiness<'_> {
        Readiness {
            scheduled_io: self,
            state: State::Init,
            waiter: UnsafeCell::new(Waiter {
                pointers: linked_list::Pointers::new(),
                waker: None,
                is_ready: false,
                interest,
                _p: PhantomPinned,
            }),
        }
    }
}

unsafe impl linked_list::Link for Waiter {
    type Handle = NonNull<Waiter>;
    type Target = Waiter;

    fn as_raw(handle: &NonNull<Waiter>) -> NonNull<Waiter> {
        *handle
    }

    unsafe fn from_raw(ptr: NonNull<Waiter>) -> NonNull<Waiter> {
        ptr
    }

    unsafe fn pointers(target: NonNull<Waiter>) -> NonNull<linked_list::Pointers<Waiter>> {
        Waiter::addr_of_pointers(target)
    }
}

// ===== impl Readiness =====

impl Future for Readiness<'_> {
    type Output = ReadyEvent;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        use std::sync::atomic::Ordering::SeqCst;

        let (scheduled_io, state, waiter) = unsafe {
            let me = self.get_unchecked_mut();
            (&me.scheduled_io, &mut me.state, &me.waiter)
        };

        loop {
            match *state {
                State::Init => {
                    // Optimistically check existing readiness
                    let curr = scheduled_io.readiness.load(SeqCst);
                    let ready = Ready::from_usize(READINESS.unpack(curr));
                    let is_shutdown = SHUTDOWN.unpack(curr) != 0;

                    // Safety: `waiter.interest` never changes
                    let interest = unsafe { (*waiter.get()).interest };
                    let ready = ready.intersection(interest);

                    if !ready.is_empty() || is_shutdown {
                        // Currently ready!
                        let tick = TICK.unpack(curr) as u8;
                        *state = State::Done;
                        return Poll::Ready(ReadyEvent {
                            tick,
                            ready,
                            is_shutdown,
                        });
                    }

                    // Wasn't ready, take the lock (and check again while locked).
                    let mut waiters = scheduled_io.waiters.lock();

                    let curr = scheduled_io.readiness.load(SeqCst);
                    let mut ready = Ready::from_usize(READINESS.unpack(curr));
                    let is_shutdown = SHUTDOWN.unpack(curr) != 0;

                    if is_shutdown {
                        ready = Ready::ALL;
                    }

                    let ready = ready.intersection(interest);

                    if !ready.is_empty() || is_shutdown {
                        // Currently ready!
                        let tick = TICK.unpack(curr) as u8;
                        *state = State::Done;
                        return Poll::Ready(ReadyEvent {
                            tick,
                            ready,
                            is_shutdown,
                        });
                    }

                    // Not ready even after locked, insert into list...

                    // Safety: called while locked
                    unsafe {
                        (*waiter.get()).waker = Some(cx.waker().clone());
                    }

                    // Insert the waiter into the linked list
                    //
                    // safety: pointers from `UnsafeCell` are never null.
                    waiters
                        .list
                        .push_front(unsafe { NonNull::new_unchecked(waiter.get()) });
                    *state = State::Waiting;
                }
                State::Waiting => {
                    // Currently in the "Waiting" state, implying the caller has
                    // a waiter stored in the waiter list (guarded by
                    // `notify.waiters`). In order to access the waker fields,
                    // we must hold the lock.

                    let waiters = scheduled_io.waiters.lock();

                    // Safety: called while locked
                    let w = unsafe { &mut *waiter.get() };

                    if w.is_ready {
                        // Our waker has been notified.
                        *state = State::Done;
                    } else {
                        // Update the waker, if necessary.
                        if !w.waker.as_ref().unwrap().will_wake(cx.waker()) {
                            w.waker = Some(cx.waker().clone());
                        }

                        return Poll::Pending;
                    }

                    // Explicit drop of the lock to indicate the scope that the
                    // lock is held. Because holding the lock is required to
                    // ensure safe access to fields not held within the lock, it
                    // is helpful to visualize the scope of the critical
                    // section.
                    drop(waiters);
                }
                State::Done => {
                    // Safety: State::Done means it is no longer shared
                    let w = unsafe { &mut *waiter.get() };

                    let curr = scheduled_io.readiness.load(Acquire);
                    let is_shutdown = SHUTDOWN.unpack(curr) != 0;

                    // The returned tick might be newer than the event
                    // which notified our waker. This is ok because the future
                    // still didn't return `Poll::Ready`.
                    let tick = TICK.unpack(curr) as u8;

                    // The readiness state could have been cleared in the meantime,
                    // but we allow the returned ready set to be empty.
                    let curr_ready = Ready::from_usize(READINESS.unpack(curr));
                    let ready = curr_ready.intersection(w.interest);

                    return Poll::Ready(ReadyEvent {
                        tick,
                        ready,
                        is_shutdown,
                    });
                }
            }
        }
    }
}

impl Drop for Readiness<'_> {
    fn drop(&mut self) {
        let mut waiters = self.scheduled_io.waiters.lock();

        // Safety: `waiter` is only ever stored in `waiters`
        unsafe {
            waiters
                .list
                .remove(NonNull::new_unchecked(self.waiter.get()))
        };
    }
}

unsafe impl Send for Readiness<'_> {}
unsafe impl Sync for Readiness<'_> {}
