use std::{pin::Pin, ptr::NonNull};
use std::sync::atomic::Ordering::SeqCst;
use crate::{loom::sync::{atomic::AtomicUsize, Mutex}, util::linked_list::{self, GuardedLinkedList, LinkedList}};

use super::{notify::Notify, Notification, NotifyOneStrategy, Waiter};

type WaitList = LinkedList<Waiter, <Waiter as linked_list::Link>::Target>;
type GuardedWaitList = GuardedLinkedList<Waiter, <Waiter as linked_list::Link>::Target>;

pub struct NotifyAll {
    state: AtomicUsize,
    waiters: Mutex<WaitList>,
}

impl NotifyAll {

    pub fn notify_first(&self) {
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
        while let EMPTY | NOTIFIED = get_state(curr) {
            // The compare-exchange from `NOTIFIED` -> `NOTIFIED` is intended. A
            // happens-before synchronization must happen between this atomic
            // operation and a task calling `notified().await`.
            let new = set_state(curr, NOTIFIED);
            let res = self.state.compare_exchange(curr, new, SeqCst, SeqCst);

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

/// List used in `Notify::notify_waiters`. It wraps a guarded linked list
/// and gates the access to it on `notify.waiters` mutex. It also empties
/// the list on drop.
struct NotifyWaitersList<'a> {
    list: GuardedWaitList,
    is_empty: bool,
    notify: &'a Notify,
}

impl<'a> NotifyWaitersList<'a> {
    fn new(
        unguarded_list: WaitList,
        guard: Pin<&'a Waiter>,
        notify: &'a Notify,
    ) -> NotifyWaitersList<'a> {
        let guard_ptr = NonNull::from(guard.get_ref());
        let list = unguarded_list.into_guarded(guard_ptr);
        NotifyWaitersList {
            list,
            is_empty: false,
            notify,
        }
    }

    /// Removes the last element from the guarded list. Modifying this list
    /// requires an exclusive access to the main list in `Notify`.
    fn pop_back_locked(&mut self, _waiters: &mut WaitList) -> Option<NonNull<Waiter>> {
        let result = self.list.pop_back();
        if result.is_none() {
            // Save information about emptiness to avoid waiting for lock
            // in the destructor.
            self.is_empty = true;
        }
        result
    }
}

impl Drop for NotifyWaitersList<'_> {
    fn drop(&mut self) {
        // If the list is not empty, we unlink all waiters from it.
        // We do not wake the waiters to avoid double panics.
        if !self.is_empty {
            let _lock_guard = self.notify.waiter.lock();
            while let Some(waiter) = self.list.pop_back() {
                // Safety: we never make mutable references to waiters.
                let waiter = unsafe { waiter.as_ref() };
                waiter.notification.store_release(Notification::All);
            }
        }
    }
}
