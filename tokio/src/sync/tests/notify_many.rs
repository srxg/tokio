use crate::sync::NotifyMany;
use std::future::Future;
use std::sync::Arc;
use std::task::Context;

#[cfg(all(target_family = "wasm", not(target_os = "wasi")))]
use wasm_bindgen_test::wasm_bindgen_test as test;


#[cfg(panic = "unwind")]
#[test]
fn notify_waiters_handles_panicking_waker() {
    use futures::task::ArcWake;

    let notify = Arc::new(NotifyMany::new());

    struct PanickingWaker(#[allow(dead_code)] Arc<NotifyMany>);

    impl ArcWake for PanickingWaker {
        fn wake_by_ref(_arc_self: &Arc<Self>) {
            panic!("waker panicked");
        }
    }

    let bad_fut = notify.notified();
    pin!(bad_fut);

    let waker = futures::task::waker(Arc::new(PanickingWaker(notify.clone())));
    let mut cx = Context::from_waker(&waker);
    let _ = bad_fut.poll(&mut cx);

    let mut futs = Vec::new();
    for _ in 0..32 {
        let mut fut = tokio_test::task::spawn(notify.notified());
        assert!(fut.poll().is_pending());
        futs.push(fut);
    }

    assert!(std::panic::catch_unwind(|| {
        notify.notify_waiters();
    })
    .is_err());

    for mut fut in futs {
        assert!(fut.poll().is_ready());
    }
}

#[test]
fn notify_simple() {
    let notify = NotifyMany::new();

    let mut fut1 = tokio_test::task::spawn(notify.notified());
    assert!(fut1.poll().is_pending());

    let mut fut2 = tokio_test::task::spawn(notify.notified());
    assert!(fut2.poll().is_pending());

    notify.notify_waiters();

    assert!(fut1.poll().is_ready());
    assert!(fut2.poll().is_ready());
}