use crate::sync::Notify;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, RawWaker, RawWakerVTable, Waker};

#[cfg(all(target_family = "wasm", not(target_os = "wasi")))]
use wasm_bindgen_test::wasm_bindgen_test as test;

#[test]
fn notify_clones_waker_before_lock() {
    const VTABLE: &RawWakerVTable = &RawWakerVTable::new(clone_w, wake, wake_by_ref, drop_w);

    unsafe fn clone_w(data: *const ()) -> RawWaker {
        let ptr = data as *const Notify;
        Arc::<Notify>::increment_strong_count(ptr);
        // Or some other arbitrary code that shouldn't be executed while the
        // Notify wait list is locked.
        (*ptr).notify_one();
        RawWaker::new(data, VTABLE)
    }

    unsafe fn drop_w(data: *const ()) {
        drop(Arc::<Notify>::from_raw(data as *const Notify));
    }

    unsafe fn wake(_data: *const ()) {
        unreachable!()
    }

    unsafe fn wake_by_ref(_data: *const ()) {
        unreachable!()
    }

    let notify = Arc::new(Notify::new());
    let notify2 = notify.clone();

    let waker =
        unsafe { Waker::from_raw(RawWaker::new(Arc::into_raw(notify2) as *const _, VTABLE)) };
    let mut cx = Context::from_waker(&waker);

    let future = notify.notified();
    pin!(future);

    // The result doesn't matter, we're just testing that we don't deadlock.
    let _ = future.poll(&mut cx);
}


#[test]
#[cfg(not(target_family = "wasm"))]
fn watch_test() {
    let rt = crate::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    rt.block_on(async {
        let (tx, mut rx) = crate::sync::watch::channel(());

        crate::spawn(async move {
            let _ = tx.send(());
        });

        let _ = rx.changed().await;
    });
}
