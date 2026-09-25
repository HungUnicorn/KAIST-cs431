use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, scope, sleep};
use std::time::Duration;

use cs431::lock::{ClhLock, Lock, McsLock, McsParkingLock, RawLock, SpinLock, TicketLock};

fn collect_lock_order<L: RawLock + Default + 'static>() -> Vec<usize> {
    let lock = Arc::new(Lock::<L, ()>::default());
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    let guard0 = lock.lock();

    let mut handles = Vec::new();
    for id in 1..=4 {
        let lock_clone = Arc::clone(&lock);
        let order_clone = Arc::clone(&order);

        let handle = thread::spawn(move || {
            let _g = lock_clone.lock();
            order_clone.lock().unwrap().push(id);
        });

        sleep(Duration::from_millis(20));
        handles.push(handle);
    }

    drop(guard0);

    for h in handles {
        h.join().unwrap();
    }

    order.lock().unwrap().clone()
}

#[test]
fn fairness_fifo() {
    assert_eq!(collect_lock_order::<TicketLock>(), [1, 2, 3, 4]);
    assert_eq!(collect_lock_order::<McsLock>(), [1, 2, 3, 4]);
    assert_eq!(collect_lock_order::<ClhLock>(), [1, 2, 3, 4]);
    assert_eq!(collect_lock_order::<McsParkingLock>(), [1, 2, 3, 4]);
}

#[test]
fn spinlock_unfair() {
    let result = collect_lock_order::<SpinLock>();
    assert_eq!(result.len(), 4);
}

#[test]
fn try_lock() {
    let lock = Lock::<SpinLock, u32>::new(42);

    let guard = lock.try_lock().unwrap();
    assert_eq!(*guard, 42);

    assert!(lock.try_lock().is_err());

    drop(guard);
    assert!(lock.try_lock().is_ok());
}

#[test]
fn blocking_waiter() {
    let lock = Arc::new(Lock::<McsParkingLock, usize>::new(0));
    let lock_clone = Arc::clone(&lock);
    let started = Arc::new(AtomicBool::new(false));
    let started_clone = Arc::clone(&started);

    let g0 = lock.lock();

    let waiter = thread::spawn(move || {
        started_clone.store(true, Ordering::Release);
        let mut g = lock_clone.lock();
        *g += 100;
    });

    while !started.load(Ordering::Acquire) {
        thread::yield_now();
    }
    sleep(Duration::from_millis(30));

    drop(g0);

    waiter.join().unwrap();
    assert_eq!(*lock.lock(), 100);
}

fn stress_test<L: RawLock + Default + Send + Sync + 'static>() {
    const NUM_THREADS: usize = 8;
    const ITERS_PER_THREAD: usize = 2000;

    let counter = Arc::new(Lock::<L, usize>::default());

    scope(|s| {
        for _ in 0..NUM_THREADS {
            let counter = &counter;
            s.spawn(move || {
                for _ in 0..ITERS_PER_THREAD {
                    let mut val = counter.lock();
                    *val += 1;
                }
            });
        }
    });

    assert_eq!(*counter.lock(), NUM_THREADS * ITERS_PER_THREAD);
}

#[test]
fn stress_concurrent() {
    stress_test::<SpinLock>();
    stress_test::<TicketLock>();
    stress_test::<McsLock>();
    stress_test::<ClhLock>();
    stress_test::<McsParkingLock>();
}
