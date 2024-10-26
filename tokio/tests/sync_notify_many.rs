#![warn(rust_2018_idioms)]
#![cfg(feature = "sync")]

#[cfg(all(target_family = "wasm", not(target_os = "wasi")))]
use wasm_bindgen_test::wasm_bindgen_test as test;

use tokio::sync::NotifyMany;

#[test]
fn notify_in_drop_after_wake() {
    use futures::task::ArcWake;
    use std::future::Future;
    use std::sync::Arc;

    let notify = Arc::new(NotifyMany::new());

    struct NotifyOnDrop(Arc<NotifyMany>);

    impl ArcWake for NotifyOnDrop {
        fn wake_by_ref(_arc_self: &Arc<Self>) {}
    }

    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            self.0.notify_waiters();
        }
    }

    let mut fut = Box::pin(async {
        notify.notified().await;
    });

    {
        let waker = futures::task::waker(Arc::new(NotifyOnDrop(notify.clone())));
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());
    }

    // Now, notifying **should not** deadlock
    notify.notify_waiters();
}