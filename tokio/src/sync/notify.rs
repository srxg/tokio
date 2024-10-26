// Allow `unreachable_pub` warnings when sync is not enabled
// due to the usage of `Notify` within the `rt` feature set.
// When this module is compiled with `sync` enabled we will warn on
// this lint. When `rt` is enabled we use `pub(crate)` which
// triggers this warning but it is safe to ignore in this case.
#![cfg_attr(not(feature = "sync"), allow(unreachable_pub, dead_code))]

use crate::loom::cell::UnsafeCell;
use crate::loom::sync::atomic::{AtomicU16, AtomicUsize};
use crate::loom::sync::Mutex;
use crate::util::linked_list::{self, LinkedList};
// use crate::util::WakeList;

use std::future::Future;
use std::marker::PhantomPinned;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::{self, Acquire, Relaxed, Release, SeqCst};
use std::task::{Context, Poll, Waker};

type WaitList = LinkedList<Waiter, <Waiter as linked_list::Link>::Target>;
// type GuardedWaitList = GuardedLinkedList<Waiter, <Waiter as linked_list::Link>::Target>;

/// Notifies a single task to wake up.
///
/// `Notify` provides a basic mechanism to notify a single task of an event.
/// `Notify` itself does not carry any data. Instead, it is to be used to signal
/// another task to perform an operation.
///
/// A `Notify` can be thought of as a [`Semaphore`] starting with 0 permits. The
/// [`notified().await`] method waits for a permit to become available, and
/// [`notify_one()`] sets a permit **if there currently are no available
/// permits**.
///
/// The synchronization details of `Notify` are similar to
/// [`thread::park`][park] and [`Thread::unpark`][unpark] from std. A [`Notify`]
/// value contains a single permit. [`notified().await`] waits for the permit to
/// be made available, consumes the permit, and resumes.  [`notify_one()`] sets
/// the permit, waking a pending task if there is one.
///
/// If `notify_one()` is called **before** `notified().await`, then the next
/// call to `notified().await` will complete immediately, consuming the permit.
/// Any subsequent calls to `notified().await` will wait for a new permit.
///
/// If `notify_one()` is called **multiple** times before `notified().await`,
/// only a **single** permit is stored. The next call to `notified().await` will
/// complete immediately, but the one after will wait for a new permit.
///
/// # Examples
///
/// Basic usage.
///
/// ```
/// use tokio::sync::Notify;
/// use std::sync::Arc;
///
/// #[tokio::main]
/// async fn main() {
///     let notify = Arc::new(Notify::new());
///     let notify2 = notify.clone();
///
///     let handle = tokio::spawn(async move {
///         notify2.notified().await;
///         println!("received notification");
///     });
///
///     println!("sending notification");
///     notify.notify_one();
///
///     // Wait for task to receive notification.
///     handle.await.unwrap();
/// }
/// ```
///
/// Unbound multi-producer single-consumer (mpsc) channel.
///
/// No wakeups can be lost when using this channel because the call to
/// `notify_one()` will store a permit in the `Notify`, which the following call
/// to `notified()` will consume.
///
/// ```
/// use tokio::sync::Notify;
///
/// use std::collections::VecDeque;
/// use std::sync::Mutex;
///
/// struct Channel<T> {
///     values: Mutex<VecDeque<T>>,
///     notify: Notify,
/// }
///
/// impl<T> Channel<T> {
///     pub fn send(&self, value: T) {
///         self.values.lock().unwrap()
///             .push_back(value);
///
///         // Notify the consumer a value is available
///         self.notify.notify_one();
///     }
///
///     // This is a single-consumer channel, so several concurrent calls to
///     // `recv` are not allowed.
///     pub async fn recv(&self) -> T {
///         loop {
///             // Drain values
///             if let Some(value) = self.values.lock().unwrap().pop_front() {
///                 return value;
///             }
///
///             // Wait for values to be available
///             self.notify.notified().await;
///         }
///     }
/// }
/// ```
///
/// Unbound multi-producer multi-consumer (mpmc) channel.
///
/// The call to [`enable`] is important because otherwise if you have two
/// calls to `recv` and two calls to `send` in parallel, the following could
/// happen:
///
///  1. Both calls to `try_recv` return `None`.
///  2. Both new elements are added to the vector.
///  3. The `notify_one` method is called twice, adding only a single
///     permit to the `Notify`.
///  4. Both calls to `recv` reach the `Notified` future. One of them
///     consumes the permit, and the other sleeps forever.
///
/// By adding the `Notified` futures to the list by calling `enable` before
/// `try_recv`, the `notify_one` calls in step three would remove the
/// futures from the list and mark them notified instead of adding a permit
/// to the `Notify`. This ensures that both futures are woken.
///
/// Notice that this failure can only happen if there are two concurrent calls
/// to `recv`. This is why the mpsc example above does not require a call to
/// `enable`.
///
/// ```
/// use tokio::sync::Notify;
///
/// use std::collections::VecDeque;
/// use std::sync::Mutex;
///
/// struct Channel<T> {
///     messages: Mutex<VecDeque<T>>,
///     notify_on_sent: Notify,
/// }
///
/// impl<T> Channel<T> {
///     pub fn send(&self, msg: T) {
///         let mut locked_queue = self.messages.lock().unwrap();
///         locked_queue.push_back(msg);
///         drop(locked_queue);
///
///         // Send a notification to one of the calls currently
///         // waiting in a call to `recv`.
///         self.notify_on_sent.notify_one();
///     }
///
///     pub fn try_recv(&self) -> Option<T> {
///         let mut locked_queue = self.messages.lock().unwrap();
///         locked_queue.pop_front()
///     }
///
///     pub async fn recv(&self) -> T {
///         let future = self.notify_on_sent.notified();
///         tokio::pin!(future);
///
///         loop {
///             // Make sure that no wakeup is lost if we get
///             // `None` from `try_recv`.
///             future.as_mut().enable();
///
///             if let Some(msg) = self.try_recv() {
///                 return msg;
///             }
///
///             // Wait for a call to `notify_one`.
///             //
///             // This uses `.as_mut()` to avoid consuming the future,
///             // which lets us call `Pin::set` below.
///             future.as_mut().await;
///
///             // Reset the future in case another call to
///             // `try_recv` got the message before us.
///             future.set(self.notify_on_sent.notified());
///         }
///     }
/// }
/// ```
///
/// [park]: std::thread::park
/// [unpark]: std::thread::Thread::unpark
/// [`notified().await`]: Notify::notified()
/// [`notify_one()`]: Notify::notify_one()
/// [`enable`]: Notified::enable()
/// [`Semaphore`]: crate::sync::Semaphore
#[derive(Debug)]
pub struct Notify {
    state: AtomicU16,
    waiters: Mutex<WaitList>,
}

#[derive(Debug)]
struct Waiter {
    /// Intrusive linked-list pointers.
    pointers: linked_list::Pointers<Waiter>,

    /// Waiting task's waker. Depending on the value of `notification`,
    /// this field is either protected by the `waiters` lock in
    /// `Notify`, or it is exclusively owned by the enclosing `Waiter`.
    waker: UnsafeCell<Option<Waker>>,

    /// Notification for this waiter. Uses 2 bits to store if and how was
    /// notified, 1 bit for storing if it was woken up using FIFO or LIFO, and
    /// the rest of it is unused.
    /// * if it's `None`, then `waker` is protected by the `waiters` lock.
    /// * if it's `Some`, then `waker` is exclusively owned by the
    ///   enclosing `Waiter` and can be accessed without locking.
    notification: AtomicNotification,

    /// Should not be `Unpin`.
    _p: PhantomPinned,
}

impl Waiter {
    fn new() -> Waiter {
        Waiter {
            pointers: linked_list::Pointers::new(),
            waker: UnsafeCell::new(None),
            notification: AtomicNotification::none(),
            _p: PhantomPinned,
        }
    }
}

generate_addr_of_methods! {
    impl<> Waiter {
        unsafe fn addr_of_pointers(self: NonNull<Self>) -> NonNull<linked_list::Pointers<Waiter>> {
            &self.pointers
        }
    }
}

// No notification.
const NOTIFICATION_NONE: usize = 0b000;

// Notification type used by `notify_one`.
const NOTIFICATION_ONE: usize = 0b001;

// Notification type used by `notify_last`.
const NOTIFICATION_LAST: usize = 0b101;

/// Notification for a `Waiter`.
/// This struct is equivalent to `Option<Notification>`, but uses
/// `AtomicUsize` inside for atomic operations.
#[derive(Debug)]
struct AtomicNotification(AtomicUsize);

impl AtomicNotification {
    fn none() -> Self {
        AtomicNotification(AtomicUsize::new(NOTIFICATION_NONE))
    }

    /// Store-release a notification according to a strategy.
    /// This method should be called exactly once.
    fn store_release(&self, strategy: NotifyOneStrategy) {
        let data: usize = match strategy {
            NotifyOneStrategy::Fifo => NOTIFICATION_ONE,
            NotifyOneStrategy::Lifo => NOTIFICATION_LAST,
        };
        self.0.store(data, Release);
    }

    fn load(&self, ordering: Ordering) -> Option<NotifyOneStrategy> {
        let data = self.0.load(ordering);
        match data {
            NOTIFICATION_NONE => None,
            NOTIFICATION_ONE => Some(NotifyOneStrategy::Fifo),
            NOTIFICATION_LAST => Some(NotifyOneStrategy::Lifo),
            _ => unreachable!(),
        }
    }

    /// Clears the notification.
    /// This method is used by a `Notified` future to consume the
    /// notification. It uses relaxed ordering and should be only
    /// used once the atomic notification is no longer shared.
    fn clear(&self) {
        self.0.store(NOTIFICATION_NONE, Relaxed);
    }
}

#[derive(Debug, PartialEq, Eq)]
#[repr(usize)]
enum NotifyOneStrategy {
    Fifo,
    Lifo,
}

/// Future returned from [`Notify::notified()`].
///
/// This future is fused, so once it has completed, any future calls to poll
/// will immediately return `Poll::Ready`.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Notified<'a> {
    /// The `Notify` being received on.
    notify: &'a Notify,

    /// The current state of the receiving process.
    state: State,

    /// Number of calls to `notify_waiters` at the time of creation.
    // notify_waiters_calls: usize,

    /// Entry in the waiter `LinkedList`.
    waiter: Waiter,
}

unsafe impl<'a> Send for Notified<'a> {}
unsafe impl<'a> Sync for Notified<'a> {}

#[derive(Debug)]
enum State {
    Init,
    Waiting,
    Done,
}

/// Initial "idle" state.
const EMPTY: u16 = 0;

/// One or more threads are currently waiting to be notified.
const WAITING: u16 = 1;

/// Pending notification.
const NOTIFIED: u16 = 2;

impl Notify {
    /// Create a new `Notify`, initialized without a permit.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::Notify;
    ///
    /// let notify = Notify::new();
    /// ```
    pub fn new() -> Notify {
        Notify {
            state: AtomicU16::new(EMPTY),
            waiters: Mutex::new(LinkedList::new()),
        }
    }

    /// Create a new `Notify`, initialized without a permit.
    ///
    /// When using the `tracing` [unstable feature], a `Notify` created with
    /// `const_new` will not be instrumented. As such, it will not be visible
    /// in [`tokio-console`]. Instead, [`Notify::new`] should be used to create
    /// an instrumented object if that is needed.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::Notify;
    ///
    /// static NOTIFY: Notify = Notify::const_new();
    /// ```
    ///
    /// [`tokio-console`]: https://github.com/tokio-rs/console
    /// [unstable feature]: crate#unstable-features
    #[cfg(not(all(loom, test)))]
    pub const fn const_new() -> Notify {
        Notify {
            state: AtomicU16::new(EMPTY),
            waiters: Mutex::const_new(LinkedList::new()),
        }
    }

    /// Wait for a notification.
    ///
    /// Equivalent to:
    ///
    /// ```ignore
    /// async fn notified(&self);
    /// ```
    ///
    /// Each `Notify` value holds a single permit. If a permit is available from
    /// an earlier call to [`notify_one()`], then `notified().await` will complete
    /// immediately, consuming that permit. Otherwise, `notified().await` waits
    /// for a permit to be made available by the next call to `notify_one()`.
    ///
    /// The `Notified` future is not guaranteed to receive wakeups from calls to
    /// `notify_one()` if it has not yet been polled. See the documentation for
    /// [`Notified::enable()`] for more details.
    ///
    /// The `Notified` future is guaranteed to receive wakeups from
    /// `notify_waiters()` as soon as it has been created, even if it has not
    /// yet been polled.
    ///
    /// [`notify_one()`]: Notify::notify_one
    /// [`Notified::enable()`]: Notified::enable
    ///
    /// # Cancel safety
    ///
    /// This method uses a queue to fairly distribute notifications in the order
    /// they were requested. Cancelling a call to `notified` makes you lose your
    /// place in the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::Notify;
    /// use std::sync::Arc;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let notify = Arc::new(Notify::new());
    ///     let notify2 = notify.clone();
    ///
    ///     tokio::spawn(async move {
    ///         notify2.notified().await;
    ///         println!("received notification");
    ///     });
    ///
    ///     println!("sending notification");
    ///     notify.notify_one();
    /// }
    /// ```
    pub fn notified(&self) -> Notified<'_> {
        Notified {
            notify: self,
            state: State::Init,
            waiter: Waiter::new(),
        }
    }

    /// Notifies the first waiting task.
    ///
    /// If a task is currently waiting, that task is notified. Otherwise, a
    /// permit is stored in this `Notify` value and the **next** call to
    /// [`notified().await`] will complete immediately consuming the permit made
    /// available by this call to `notify_one()`.
    ///
    /// At most one permit may be stored by `Notify`. Many sequential calls to
    /// `notify_one` will result in a single permit being stored. The next call to
    /// `notified().await` will complete immediately, but the one after that
    /// will wait.
    ///
    /// [`notified().await`]: Notify::notified()
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::Notify;
    /// use std::sync::Arc;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let notify = Arc::new(Notify::new());
    ///     let notify2 = notify.clone();
    ///
    ///     tokio::spawn(async move {
    ///         notify2.notified().await;
    ///         println!("received notification");
    ///     });
    ///
    ///     println!("sending notification");
    ///     notify.notify_one();
    /// }
    /// ```
    // Alias for old name in 0.x
    #[cfg_attr(docsrs, doc(alias = "notify"))]
    pub fn notify_one(&self) {
        self.notify_with_strategy(NotifyOneStrategy::Fifo);
    }

    /// Notifies the last waiting task.
    ///
    /// This function behaves similar to `notify_one`. The only difference is that it wakes
    /// the most recently added waiter instead of the oldest waiter.
    ///
    /// Check the [`notify_one()`] documentation for more info and
    /// examples.
    ///
    /// [`notify_one()`]: Notify::notify_one
    pub fn notify_last(&self) {
        self.notify_with_strategy(NotifyOneStrategy::Lifo);
    }

    fn notify_with_strategy(&self, strategy: NotifyOneStrategy) {
        // Load the current state
        let mut curr = self.state.load(SeqCst);

        // If the state is `EMPTY`, transition to `NOTIFIED` and return.
        while let EMPTY | NOTIFIED = curr {
            // The compare-exchange from `NOTIFIED` -> `NOTIFIED` is intended. A
            // happens-before synchronization must happen between this atomic
            // operation and a task calling `notified().await`.
            let res = self.state.compare_exchange(curr, NOTIFIED, SeqCst, SeqCst);

            match res {
                // No waiters, no further work to do
                Ok(_) => return,
                Err(actual) => {
                    curr = actual;
                }
            }
        }

        // There are waiters, the lock must be acquired to notify.
        let mut waiters = self.waiters.lock();

        // The state must be reloaded while the lock is held. The state may only
        // transition out of WAITING while the lock is held.
        curr = self.state.load(SeqCst);

        if let Some(waker) = notify_locked(&mut waiters, &self.state, curr, strategy) {
            drop(waiters);
            waker.wake();
        }
    }
}

impl Default for Notify {
    fn default() -> Notify {
        Notify::new()
    }
}

impl UnwindSafe for Notify {}
impl RefUnwindSafe for Notify {}

fn notify_locked(
    waiters: &mut WaitList,
    state: &AtomicU16,
    curr: u16,
    strategy: NotifyOneStrategy,
) -> Option<Waker> {
    match curr {
        EMPTY | NOTIFIED => {
            let res = state.compare_exchange(curr, NOTIFIED, SeqCst, SeqCst);

            match res {
                Ok(_) => None,
                Err(actual) => {
                    assert!(actual == EMPTY || actual == NOTIFIED);
                    state.store(NOTIFIED, SeqCst);
                    None
                }
            }
        }
        WAITING => {
            // At this point, it is guaranteed that the state will not
            // concurrently change as holding the lock is required to
            // transition **out** of `WAITING`.
            //
            // Get a pending waiter using one of the available dequeue strategies.
            let waiter = match strategy {
                NotifyOneStrategy::Fifo => waiters.pop_back().unwrap(),
                NotifyOneStrategy::Lifo => waiters.pop_front().unwrap(),
            };

            // Safety: we never make mutable references to waiters.
            let waiter = unsafe { waiter.as_ref() };

            // Safety: we hold the lock, so we can access the waker.
            let waker = unsafe { waiter.waker.with_mut(|waker| (*waker).take()) };

            // This waiter is unlinked and will not be shared ever again, release it.
            waiter
                .notification
                .store_release(strategy);

            if waiters.is_empty() {
                // As this the **final** waiter in the list, the state
                // must be transitioned to `EMPTY`. As transitioning
                // **from** `WAITING` requires the lock to be held, a
                // `store` is sufficient.
                state.store(EMPTY, SeqCst);
            }
            waker
        }
        _ => unreachable!(),
    }
}

// ===== impl Notified =====

impl Notified<'_> {
    /// Adds this future to the list of futures that are ready to receive
    /// wakeups from calls to [`notify_one`].
    ///
    /// Polling the future also adds it to the list, so this method should only
    /// be used if you want to add the future to the list before the first call
    /// to `poll`. (In fact, this method is equivalent to calling `poll` except
    /// that no `Waker` is registered.
    ///
    /// This method returns true if the `Notified` is ready. This happens in the
    /// following situations:
    ///
    ///  1. This is the first call to `enable` or `poll` on this future, and the
    ///     `Notify` was holding a permit from a previous call to `notify_one`.
    ///     The call consumes the permit in that case.
    ///  3. The future has previously been enabled or polled, and it has since
    ///     then been marked ready by either consuming a permit from the
    ///     `Notify`, or by a call to `notify_one` that
    ///     removed it from the list of futures ready to receive wakeups.
    ///
    /// If this method returns true, any future calls to poll on the same future
    /// will immediately return `Poll::Ready`.
    ///
    /// # Examples
    ///
    /// Unbound multi-producer multi-consumer (mpmc) channel.
    ///
    /// The call to `enable` is important because otherwise if you have two
    /// calls to `recv` and two calls to `send` in parallel, the following could
    /// happen:
    ///
    ///  1. Both calls to `try_recv` return `None`.
    ///  2. Both new elements are added to the vector.
    ///  3. The `notify_one` method is called twice, adding only a single
    ///     permit to the `Notify`.
    ///  4. Both calls to `recv` reach the `Notified` future. One of them
    ///     consumes the permit, and the other sleeps forever.
    ///
    /// By adding the `Notified` futures to the list by calling `enable` before
    /// `try_recv`, the `notify_one` calls in step three would remove the
    /// futures from the list and mark them notified instead of adding a permit
    /// to the `Notify`. This ensures that both futures are woken.
    ///
    /// ```
    /// use tokio::sync::Notify;
    ///
    /// use std::collections::VecDeque;
    /// use std::sync::Mutex;
    ///
    /// struct Channel<T> {
    ///     messages: Mutex<VecDeque<T>>,
    ///     notify_on_sent: Notify,
    /// }
    ///
    /// impl<T> Channel<T> {
    ///     pub fn send(&self, msg: T) {
    ///         let mut locked_queue = self.messages.lock().unwrap();
    ///         locked_queue.push_back(msg);
    ///         drop(locked_queue);
    ///
    ///         // Send a notification to one of the calls currently
    ///         // waiting in a call to `recv`.
    ///         self.notify_on_sent.notify_one();
    ///     }
    ///
    ///     pub fn try_recv(&self) -> Option<T> {
    ///         let mut locked_queue = self.messages.lock().unwrap();
    ///         locked_queue.pop_front()
    ///     }
    ///
    ///     pub async fn recv(&self) -> T {
    ///         let future = self.notify_on_sent.notified();
    ///         tokio::pin!(future);
    ///
    ///         loop {
    ///             // Make sure that no wakeup is lost if we get
    ///             // `None` from `try_recv`.
    ///             future.as_mut().enable();
    ///
    ///             if let Some(msg) = self.try_recv() {
    ///                 return msg;
    ///             }
    ///
    ///             // Wait for a call to `notify_one`.
    ///             //
    ///             // This uses `.as_mut()` to avoid consuming the future,
    ///             // which lets us call `Pin::set` below.
    ///             future.as_mut().await;
    ///
    ///             // Reset the future in case another call to
    ///             // `try_recv` got the message before us.
    ///             future.set(self.notify_on_sent.notified());
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// [`notify_one`]: Notify::notify_one()
    pub fn enable(self: Pin<&mut Self>) -> bool {
        self.poll_notified(None).is_ready()
    }

    /// A custom `project` implementation is used in place of `pin-project-lite`
    /// as a custom drop implementation is needed.
    fn project(self: Pin<&mut Self>) -> (&Notify, &mut State, &Waiter) {
        unsafe {
            // Safety: `notify`, `state` are `Unpin`.

            is_unpin::<&Notify>();
            is_unpin::<State>();

            let me = self.get_unchecked_mut();

            (
                me.notify,
                &mut me.state,
                &me.waiter,
            )
        }
    }

    fn poll_notified(self: Pin<&mut Self>, waker: Option<&Waker>) -> Poll<()> {
        let (notify, state, waiter) = self.project();

        'outer_loop: loop {
            match *state {
                State::Init => {
                    // Optimistically try acquiring a pending notification
                    let res = notify.state.compare_exchange(
                        NOTIFIED,
                        EMPTY,
                        SeqCst,
                        SeqCst,
                    );

                    if res.is_ok() {
                        // Acquired the notification
                        *state = State::Done;
                        continue 'outer_loop;
                    }

                    // Clone the waker before locking, a waker clone can be
                    // triggering arbitrary code.
                    let waker = waker.cloned();

                    // Acquire the lock and attempt to transition to the waiting
                    // state.
                    let mut waiters = notify.waiters.lock();

                    // Reload the state with the lock held
                    let mut curr = notify.state.load(SeqCst);

                    // if notify_waiters has been called after the future
                    // was created, then we are done
                    // if get_num_notify_waiters_calls(curr) != *notify_waiters_calls {
                    //     *state = State::Done;
                    //     continue 'outer_loop;
                    // }

                    // Transition the state to WAITING.
                    loop {
                        match curr {
                            EMPTY => {
                                // Transition to WAITING
                                let res = notify.state.compare_exchange(
                                    EMPTY,
                                    WAITING,
                                    SeqCst,
                                    SeqCst,
                                );

                                if let Err(actual) = res {
                                    assert_eq!(actual, NOTIFIED);
                                    curr = actual;
                                } else {
                                    break;
                                }
                            }
                            WAITING => break,
                            NOTIFIED => {
                                // Try consuming the notification
                                let res = notify.state.compare_exchange(
                                    NOTIFIED,
                                    EMPTY,
                                    SeqCst,
                                    SeqCst,
                                );

                                match res {
                                    Ok(_) => {
                                        // Acquired the notification
                                        *state = State::Done;
                                        continue 'outer_loop;
                                    }
                                    Err(actual) => {
                                        assert_eq!(actual, EMPTY);
                                        curr = actual;
                                    }
                                }
                            }
                            _ => unreachable!(),
                        }
                    }

                    let mut old_waker = None;
                    if waker.is_some() {
                        // Safety: called while locked.
                        //
                        // The use of `old_waiter` here is not necessary, as the field is always
                        // None when we reach this line.
                        unsafe {
                            old_waker =
                                waiter.waker.with_mut(|v| std::mem::replace(&mut *v, waker));
                        }
                    }

                    // Insert the waiter into the linked list
                    waiters.push_front(NonNull::from(waiter));

                    *state = State::Waiting;

                    drop(waiters);
                    drop(old_waker);

                    return Poll::Pending;
                }
                State::Waiting => {
                    #[cfg(tokio_taskdump)]
                    if let Some(waker) = waker {
                        let mut ctx = Context::from_waker(waker);
                        std::task::ready!(crate::trace::trace_leaf(&mut ctx));
                    }

                    if waiter.notification.load(Acquire).is_some() {
                        // Safety: waiter is already unlinked and will not be shared again,
                        // so we have an exclusive access to `waker`.
                        drop(unsafe { waiter.waker.with_mut(|waker| (*waker).take()) });

                        waiter.notification.clear();
                        *state = State::Done;
                        return Poll::Ready(());
                    }

                    // Our waiter was not notified, implying it is still stored in a waiter
                    // list (guarded by `notify.waiters`). In order to access the waker
                    // fields, we must acquire the lock.

                    let mut old_waker = None;
                    let waiters = notify.waiters.lock();

                    // We hold the lock and notifications are set only with the lock held,
                    // so this can be relaxed, because the happens-before relationship is
                    // established through the mutex.
                    if waiter.notification.load(Relaxed).is_some() {
                        // Safety: waiter is already unlinked and will not be shared again,
                        // so we have an exclusive access to `waker`.
                        old_waker = unsafe { waiter.waker.with_mut(|waker| (*waker).take()) };

                        waiter.notification.clear();

                        // Drop the old waker after releasing the lock.
                        drop(waiters);
                        drop(old_waker);

                        *state = State::Done;
                        return Poll::Ready(());
                    }

                    // Safety: we hold the lock, so we can modify the waker
                    unsafe {
                        waiter.waker.with_mut(|v| {
                            if let Some(waker) = waker {
                                let should_update = match &*v {
                                    Some(current_waker) => !current_waker.will_wake(waker),
                                    None => true,
                                };
                                if should_update {
                                    old_waker = std::mem::replace(&mut *v, Some(waker.clone()));
                                }
                            }
                        });
                    }

                    // Drop the old waker after releasing the lock.
                    drop(waiters);
                    drop(old_waker);

                    return Poll::Pending;
                }
                State::Done => {
                    #[cfg(tokio_taskdump)]
                    if let Some(waker) = waker {
                        let mut ctx = Context::from_waker(waker);
                        std::task::ready!(crate::trace::trace_leaf(&mut ctx));
                    }
                    return Poll::Ready(());
                }
            }
        }
    }
    
}

impl Future for Notified<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.poll_notified(Some(cx.waker()))
    }
}

impl Drop for Notified<'_> {
    fn drop(&mut self) {
        // Safety: The type only transitions to a "Waiting" state when pinned.
        let (notify, state, waiter) = unsafe { Pin::new_unchecked(self).project() };
        
        // This is where we ensure safety. The `Notified` value is being
        // dropped, which means we must ensure that the waiter entry is no
        // longer stored in the linked list.
        if matches!(*state, State::Waiting) {
            let mut waiters = notify.waiters.lock();
            let notify_state = notify.state.load(SeqCst);

            // We hold the lock, so this field is not concurrently accessed by
            // `notify_*` functions and we can use the relaxed ordering.
            let notification = waiter.notification.load(Relaxed);

            // remove the entry from the list (if not already removed)
            //
            // Safety: we hold the lock, so we have an exclusive access to every list the
            // waiter may be contained in.
            // If the node is not contained in the `waiters`
            // list, then it is contained by a guarded list used by `notify_waiters`.
            unsafe { waiters.remove(NonNull::from(waiter)) };

            if waiters.is_empty() && notify_state == WAITING {
                notify.state.store(EMPTY, SeqCst);
            }

            // See if the node was notified but not received. In this case, if
            // the notification was triggered via `notify_one`, it must be sent
            // to the next waiter.
            if let Some(strategy) = notification {
                if let Some(waker) =
                    notify_locked(&mut waiters, &notify.state, notify_state, strategy)
                {
                    drop(waiters);
                    waker.wake();
                }
            }
        }
    }
}

/// # Safety
///
/// `Waiter` is forced to be !Unpin.
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

fn is_unpin<T: Unpin>() {}
