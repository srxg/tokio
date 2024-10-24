use core::task::Waker;
use std::sync::atomic::Ordering::Relaxed;
use std::{marker::PhantomPinned, sync::atomic::Ordering};
use std::sync::atomic::Ordering::Release;
use crate::{loom::{cell::UnsafeCell, sync::atomic::AtomicUsize}, util::linked_list};

pub mod notify_all;
pub mod notify;


// No notification.
const NOTIFICATION_NONE: usize = 0b000;

// Notification type used by `notify_one`.
const NOTIFICATION_ONE: usize = 0b001;

// Notification type used by `notify_last`.
const NOTIFICATION_LAST: usize = 0b101;

// Notification type used by `notify_waiters`.
const NOTIFICATION_ALL: usize = 0b010;

/// Notification for a `Waiter`.
/// This struct is equivalent to `Option<Notification>`, but uses
/// `AtomicUsize` inside for atomic operations.
#[derive(Debug)]
struct AtomicNotification(AtomicUsize);

impl AtomicNotification {
    fn none() -> Self {
        AtomicNotification(AtomicUsize::new(NOTIFICATION_NONE))
    }

    /// Store-release a notification.
    /// This method should be called exactly once.
    fn store_release(&self, notification: Notification) {
        let data: usize = match notification {
            Notification::All => NOTIFICATION_ALL,
            Notification::One(NotifyOneStrategy::Fifo) => NOTIFICATION_ONE,
            Notification::One(NotifyOneStrategy::Lifo) => NOTIFICATION_LAST,
        };
        self.0.store(data, Release);
    }

    fn load(&self, ordering: Ordering) -> Option<Notification> {
        let data = self.0.load(ordering);
        match data {
            NOTIFICATION_NONE => None,
            NOTIFICATION_ONE => Some(Notification::One(NotifyOneStrategy::Fifo)),
            NOTIFICATION_LAST => Some(Notification::One(NotifyOneStrategy::Lifo)),
            NOTIFICATION_ALL => Some(Notification::All),
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

#[derive(Debug)]
pub(self) struct Waiter {
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

#[derive(Debug, PartialEq, Eq)]
#[repr(usize)]
enum NotifyOneStrategy {
    Fifo,
    Lifo,
}

#[derive(Debug, PartialEq, Eq)]
#[repr(usize)]
enum Notification {
    One(NotifyOneStrategy),
    All,
}