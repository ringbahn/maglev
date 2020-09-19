use std::cell::{Cell, UnsafeCell};
use std::cmp;
use std::future::Future;
use std::io;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::ptr;
use std::task::{Poll, Context};
use std::thread::{self, ThreadId};
use std::time::Duration;

use event_listener::{Event, EventListener};
use futures_core::ready;
use ringbahn::drive::{Drive, Completion};
use iou::*;
use waker_fn::waker_fn;

const ENTRIES: u32 = 32;
thread_local!(static PROACTOR: Proactor = Proactor::new(ENTRIES).unwrap());

pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    PROACTOR.with(|proactor| proactor.block_on(future))
}

#[derive(Default)]
pub struct Driver {
    listener: Option<EventListener>,
    prepared_on_thread: Option<ThreadId>,
}

impl Driver {
    fn prepared_on(&self, thread: ThreadId) -> bool {
        self.prepared_on_thread.map_or(false, |id| id == thread)
    }
}

impl Clone for Driver {
    fn clone(&self) -> Driver {
        Driver::default()
    }
}

impl Drive for Driver {
    fn poll_prepare<'cx>(
        mut self: Pin<&mut Self>,
        ctx: &mut Context<'cx>,
        count: u32,
        prepare: impl FnOnce(SQEs<'_>, &mut Context<'cx>) -> Completion<'cx>,
    ) -> Poll<Completion<'cx>> {
        let thread = thread::current().id();

        if self.prepared_on(thread) {
            if let Some(listener) = &mut self.listener {
                ready!(Pin::new(listener).poll(ctx));
            }
        } else {
            self.listener = None;
            self.prepared_on_thread = Some(thread);
        }

        PROACTOR.with(|proactor| {
            match proactor.prepare(count) {
                Ok(sqes)            => {
                    let completion = prepare(sqes, ctx);
                    Poll::Ready(completion)
                }
                Err(new_listener)   => {
                    self.listener = Some(new_listener);
                    Poll::Pending
                }
            }
        })
    }

    fn poll_submit(self: Pin<&mut Self>, _: &mut Context<'_>, _: bool)
        -> Poll<io::Result<u32>>
    {
        Poll::Ready(PROACTOR.with(|proactor| {
            if !proactor.blocked_on() {
                proactor.submit(0)
            } else {
                Ok(0)
            }
        }))
    }
}

struct Proactor {
    in_flight: Semaphore,
    in_queue: Event,
    ring: UnsafeCell<IoUring>,
    blocked_on: Cell<bool>,
}

impl Proactor {
    fn new(events: u32) -> io::Result<Proactor> {
        Ok(Proactor {
            in_flight: Semaphore::new(events << 1),
            in_queue: Event::new(),
            ring: UnsafeCell::new(IoUring::new(events)?),
            blocked_on: Cell::new(false),
        })
    }

    fn blocked_on(&self) -> bool {
        self.blocked_on.get()
    }

    fn any_in_flight(&self) -> bool {
        self.in_flight.active() > 0 
    }

    fn block_on<T>(&self, mut future: impl Future<Output = T>) -> T {
        const ZERO: Duration = Duration::from_secs(0);

        // First, indicate that we are blocked on to ensure that
        // the driver never eagerly submits.
        let prev = self.blocked_on.replace(true);

        // Our waker will simply unpark the thread using the parking
        // crate.
        let (parker, unparker) = parking::pair();
        let waker = waker_fn(move || { unparker.unpark(); });
        let ctx = &mut Context::from_waker(&waker);

        // Pin the future in place. By shadowing it we guarantee it
        // will never be moved again.
        let mut future = unsafe { Pin::new_unchecked(&mut future) };

        'poll: loop {
            // First, poll the future. If it has completed, we exit
            // the block_on function.
            if let Poll::Ready(t) = future.as_mut().poll(ctx) {
                self.blocked_on.set(prev);
                return t;
            }

            // If the parker has already been unparked, we submit any
            // events to the kernel without blocking, then continue
            // to poll the future.
            if parker.park_timeout(ZERO) {
                self.submit(0).expect("TODO handle submission errors");
                continue 'poll;
            }

            // If there are any submissions that have not yet been
            // completed, submit events to the kernel and block until
            // any of them have been completed. Then, if we have been
            // unparked, continue polling.
            //
            // We continue this repeatedly until either we have been
            // unparked or there are no more live submissions that we
            // could complete.
            while self.any_in_flight() {
                self.submit(1).expect("TODO handle submission errors");

                if parker.park_timeout(ZERO) {
                    continue 'poll;
                }
            }

            // If there are no events to be completed and no work to do,
            // we just park the thread until it gets unparked.
            parker.park();
        }
    }

    fn prepare(&self, count: u32) -> Result<SQEs<'_>, EventListener> {
        if self.in_flight.leases() >= count {
            let ring = unsafe { &mut *self.ring.get() };
            if let Some(sqes) = ring.prepare_sqes(count) {
                self.in_flight.lease(count);
                Ok(sqes)
            } else {
                Err(self.in_queue.listen())
            }
        } else {
            Err(self.in_flight.listen())
        }
    }

    fn submit(&self, wait_for: u32) -> io::Result<u32> {
        use uring_sys::*;

        unsafe {
            let ring = &mut *self.ring.get();
            // First, submit events to the kernel. If `wait_for` is not `0`, this will
            // block until that many events complete.
            let submitted = ring.submit_sqes_and_wait(wait_for)?;
            self.in_queue.notify_additional(submitted as usize);

            // Then, we events that have completed. These events will be processed in
            // batches of `4`, to reduce the number of synchronizations with the kernel
            // during that process.
            //
            // The conditional is a bit tricky: if ready is not greater than 4, we check
            // how many are ready now, and then check if ready is greater than 0.
            let mut ready = 0;
            while ready >= 4 || { ready = ring.cq_ready(); ready > 0 }  {
                let ring = ring.raw_mut();

                // First, we call peek_batch_cqe to get up to 4 pointers into the CQ of
                // completed events.
                let count = cmp::min(ready, 4);
                let mut cqes: MaybeUninit<[*mut io_uring_cqe; 4]> = MaybeUninit::uninit();
                let count = io_uring_peek_batch_cqe(ring, cqes.as_mut_ptr() as _, count);
                debug_assert!(count == cmp::min(ready, 4));

                // Then, we copy all of the data from the CQEs out of the CQ into the
                // stack.
                let cqe_slot: [*mut io_uring_cqe; 4] = cqes.assume_init();
                let mut cqes: [Option<CQE>; 4] = [None, None, None, None];
                for n in 0..(count as usize) {
                    cqes[n] = Some(CQE::from_raw(ptr::read(cqe_slot[n])));
                }

                // Now that we have the CQE data, we do three things in this specific
                // order:
                // * First, we advance the CQ, so that the slots of those CQEs can be
                // reused.
                // * Second, we release that many leases on our Semaphore, so that
                // tasks that are blocked on the semaphore can proceed.
                // * Third, we complete those CQEs, waking any tasks that are blocked
                // on those events can proceed.
                //
                // It's important that these occur in this order, so that there will
                // be room in the CQ for all events and so that events blocked on the
                // semaphore will not be starved by events that have just completed
                // their work (assuming the executor runs task in a FIFO order).
                uring_sys::io_uring_cq_advance(ring, count);
                self.in_flight.release(count);
                cqes.iter_mut().take(count as usize).filter_map(Option::take).for_each(ringbahn::drive::complete);

                ready -= count;
            }

            Ok(submitted)
        }
    }
}

struct Semaphore {
    max: u32,
    leases: Cell<u32>,
    event: Event,
}

impl Semaphore {
    fn new(leases: u32) -> Semaphore {
        Semaphore {
            max: leases,
            leases: Cell::new(leases),
            event: Event::new(),
        }
    }

    fn lease(&self, n: u32) {
        self.leases.set(self.leases() - n)
    }

    fn listen(&self) -> EventListener {
        self.event.listen()
    }

    fn release(&self, n: u32) {
        let leases = self.leases();
        match leases.checked_add(n) {
            Some(leases)   => {
                self.event.notify_additional(n as usize);
                self.leases.set(leases);
            }
            None           => std::process::abort(),
        }
    }

    fn active(&self) -> u32 {
        self.max - self.leases()
    }

    fn leases(&self) -> u32 {
        self.leases.get()
    }
}
