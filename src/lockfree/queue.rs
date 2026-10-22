//! Michael-Scott lock-free queue.
//!
//! Usable with any number of producers and consumers.
//!
//! Michael and Scott.  Simple, Fast, and Practical Non-Blocking and Blocking Concurrent Queue
//! Algorithms.  PODC 1996.  <http://dl.acm.org/citation.cfm?id=248106>

use core::mem::{self, MaybeUninit};
use core::sync::atomic::Ordering::*;

use crossbeam_epoch::{Atomic, Guard, Owned, Shared};
use crossbeam_utils::CachePadded;

/// Michael-Scott queue.
// The representation here is a singly-linked list, with a sentinel node at the front. In general
// the `tail` pointer may lag behind the actual tail.
#[derive(Debug)]
pub struct Queue<T> {
    head: CachePadded<Atomic<Node<T>>>,
    tail: CachePadded<Atomic<Node<T>>>,
}

#[derive(Debug)]
struct Node<T> {
    /// The place in which a value of type `T` can be stored.
    ///
    /// The type of `data` is `MaybeUninit<T>` because a `Node<T>` doesn't always contain a `T`.
    /// For example, the initial sentinel node in a queue never contains a value: its data is
    /// always uninitialized. Other nodes start their life with a push operation and contain a
    /// value until it gets popped out.
    data: MaybeUninit<T>,

    next: Atomic<Node<T>>,
}

// Any particular `T` should never be accessed concurrently, so no need for `Sync`.
unsafe impl<T: Send> Sync for Queue<T> {}
unsafe impl<T: Send> Send for Queue<T> {}

impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Queue<T> {
    /// Create a new, empty queue.
    pub fn new() -> Self {
        let sentinel = Box::into_raw(Box::new(Node {
            data: MaybeUninit::uninit(),
            next: Atomic::null(),
        }))
        .cast_const();

        Self {
            head: CachePadded::new(sentinel.into()),
            tail: CachePadded::new(sentinel.into()),
        }
    }

    /// Adds `t` to the back of the queue.
    pub fn push(&self, t: T, guard: &mut Guard) {
        let mut new = Owned::new(Node {
            data: MaybeUninit::new(t),
            next: Atomic::null(),
        });

        loop {
            // We push onto the tail, so we'll start optimistically by looking there first.
            let tail = self.tail.load(Acquire, guard);

            // Attempt to push onto the `tail` snapshot; fails if `tail.next` has changed.
            let tail_ref = unsafe { tail.deref() };
            let next = tail_ref.next.load(Acquire, guard);

            // If `tail` is not the actual tail, try to "help" by moving the tail pointer forward.
            if !next.is_null() {
                let _ = self
                    .tail
                    .compare_exchange(tail, next, Release, Relaxed, guard);
                continue;
            }

            // looks like the actual tail; attempt to link at `tail.next`.
            match tail_ref
                .next
                .compare_exchange(Shared::null(), new, Release, Relaxed, guard)
            {
                Ok(new) => {
                    // try to move the tail pointer forward.
                    let _ = self
                        .tail
                        .compare_exchange(tail, new, Release, Relaxed, guard);
                    break;
                }
                Err(e) => new = e.new,
            }
            guard.repin();
        }
    }

    /// Attempts to dequeue from the front.
    ///
    /// Returns `None` if the queue is observed to be empty.
    pub fn try_pop(&self, guard: &mut Guard) -> Option<T> {
        loop {
            let head = self.head.load(Acquire, guard);
            let next = unsafe { head.deref() }.next.load(Acquire, guard);

            let next_ref = unsafe { next.as_ref() }?;

            // Moves `tail` if it's stale. Relaxed load is enough because if tail == head, then the
            // messages for that node are already acquired.
            let tail = self.tail.load(Relaxed, guard);
            if tail == head {
                let _ = self
                    .tail
                    .compare_exchange(tail, next, Release, Relaxed, guard);
            }

            // After the above load & CAS, the thread view ensures that the index of tail is greater
            // than that of current head. We relase that view to the head with the below CAS,
            // ensuring that the index of the new head is less than or equal to that of the tail.
            //
            // Note: this reasoning is also done in SC memory regarding index of head and tail,
            // albeit simpler.
            if self
                .head
                .compare_exchange(head, next, Release, Relaxed, guard)
                .is_ok()
            {
                // Since the above `compare_exchange()` succeeded, `head` is detached from `self` so
                // is unreachable from other threads.

                // SAFETY: `next` will never be the sentinel node, since it is the node after
                // `head`. Hence, it must have been a node made in `push()`, which is initialized.
                //
                // Also, we are returning ownership of `data` in `next` by making a copy of it via
                // `assume_init_read()`. This is safe as no other thread has access to `data` after
                // `head` is unreachable, so the ownership of `data` in `next` will never be used
                // again as it is now a sentinel node.
                let result = unsafe { next_ref.data.assume_init_read() };

                // SAFETY: `head` is unreachable, and we no longer access `head`. We destroy `head`
                // after the final access to `next` above to ensure that `next` is also destroyed
                // after.
                unsafe { guard.defer_destroy(head) };

                return Some(result);
            }
            guard.repin();
        }
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        // Destroy the sentinel node.

        let sentinel = mem::take(&mut *self.head);
        // SAFETY: `pop()` never dropped the sentinel node so it is still valid.
        let mut o_curr = unsafe { sentinel.into_owned() }.into_box().next;

        // Destroy and deallocate `data` for the rest of the nodes.

        // SAFETY: All non-null nodes made were valid, and we have unique ownership via `&mut self`.
        while let Some(curr) = unsafe { o_curr.try_into_owned() }.map(Owned::into_box) {
            // SAFETY: Not sentinel node, so `data` is valid.
            drop(unsafe { curr.data.assume_init() });
            o_curr = curr.next;
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread::scope;

    use crossbeam_epoch::pin;

    use super::*;

    struct Queue<T> {
        queue: super::Queue<T>,
    }

    impl<T> Queue<T> {
        pub fn new() -> Queue<T> {
            Queue {
                queue: super::Queue::new(),
            }
        }

        pub fn push(&self, t: T) {
            let guard = &mut pin();
            self.queue.push(t, guard);
        }

        pub fn is_empty(&self) -> bool {
            let guard = &pin();
            let head = self.queue.head.load(Acquire, guard);
            let next = unsafe { head.deref() }.next.load(Acquire, guard);
            next.is_null()
        }

        pub fn try_pop(&self) -> Option<T> {
            let guard = &mut pin();
            self.queue.try_pop(guard)
        }

        pub fn pop(&self) -> T {
            loop {
                if let Some(t) = self.try_pop() {
                    return t;
                }
            }
        }
    }

    const CONC_COUNT: i64 = 1000000;

    #[test]
    fn try_pop_empty_returns_none() {
        let q: Queue<i64> = Queue::new();
        assert_eq!(q.try_pop(), None);
    }

    #[test]
    fn is_empty_on_new_returns_true() {
        let q: Queue<i64> = Queue::new();
        assert!(q.is_empty());
    }

    #[test]
    fn push_makes_queue_non_empty() {
        let q: Queue<i64> = Queue::new();
        q.push(42);
        assert!(!q.is_empty());
    }

    #[test]
    fn single_push_try_pop_returns_element_and_becomes_empty() {
        let q: Queue<i64> = Queue::new();
        assert!(q.is_empty());
        q.push(37);
        assert!(!q.is_empty());
        assert_eq!(q.try_pop(), Some(37));
        assert!(q.is_empty());
    }

    #[test]
    fn two_pushes_try_pop_returns_fifo_order_and_becomes_empty() {
        let q: Queue<i64> = Queue::new();
        assert!(q.is_empty());
        q.push(37);
        q.push(48);
        assert_eq!(q.try_pop(), Some(37));
        assert!(!q.is_empty());
        assert_eq!(q.try_pop(), Some(48));
        assert!(q.is_empty());
    }

    #[test]
    fn single_push_blocking_pop_returns_element_and_becomes_empty() {
        let q: Queue<i64> = Queue::new();
        assert!(q.is_empty());
        q.push(37);
        assert!(!q.is_empty());
        assert_eq!(q.pop(), 37);
        assert!(q.is_empty());
    }

    #[test]
    fn two_pushes_blocking_pop_returns_elements_in_fifo_order() {
        let q: Queue<i64> = Queue::new();
        q.push(37);
        q.push(48);
        assert_eq!(q.pop(), 37);
        assert_eq!(q.pop(), 48);
    }

    #[test]
    fn push_multiple_makes_is_empty_false_and_allows_pop() {
        let q: Queue<i64> = Queue::new();
        q.push(20);
        q.push(20);
        assert!(!q.is_empty());
        assert!(q.try_pop().is_some());
    }

    #[test]
    fn fifo_order_preserved() {
        let q: Queue<i64> = Queue::new();
        for i in 0..100 {
            q.push(i);
        }
        for i in 0..100 {
            assert_eq!(q.try_pop(), Some(i));
        }
        assert!(q.is_empty());
    }

    #[test]
    fn interleaved_push_pop_preserves_fifo_order() {
        let q: Queue<i64> = Queue::new();
        q.push(1);
        q.push(2);
        assert_eq!(q.try_pop(), Some(1));

        q.push(3);
        assert_eq!(q.try_pop(), Some(2));
        assert_eq!(q.try_pop(), Some(3));
        assert_eq!(q.try_pop(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn sequential_200_pushes_try_pop_all_in_fifo_order_and_becomes_empty() {
        let q: Queue<i64> = Queue::new();
        assert!(q.is_empty());
        for i in 0..200 {
            q.push(i)
        }
        assert!(!q.is_empty());
        for i in 0..200 {
            assert_eq!(q.try_pop(), Some(i));
        }
        assert!(q.is_empty());
    }

    #[test]
    fn sequential_200_pushes_blocking_pop_all_in_fifo_order_and_becomes_empty() {
        let q: Queue<i64> = Queue::new();
        assert!(q.is_empty());
        for i in 0..200 {
            q.push(i)
        }
        assert!(!q.is_empty());
        for i in 0..200 {
            assert_eq!(q.pop(), i);
        }
        assert!(q.is_empty());
    }

    #[test]
    fn drop_empty_queue_does_not_crash_or_read_uninit() {
        let q: Queue<String> = Queue::new();
        drop(q);
    }

    #[test]
    fn drop_with_remaining_elements_does_not_crash() {
        let q: Queue<i64> = Queue::new();
        for i in 0..100 {
            q.push(i);
        }
    }

    #[test]
    fn drop_calls_destructor_for_all_remaining_elements() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropCounter;
        impl Drop for DropCounter {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }

        DROP_COUNT.store(0, Ordering::Relaxed);
        {
            let q = Queue::new();
            for _ in 0..50 {
                q.push(DropCounter);
            }
            for _ in 0..10 {
                q.try_pop();
            }
        }
        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 50);
    }

    #[test]
    fn concurrent_push_then_pop_count_matches() {
        let q = Queue::new();
        let threads = 8;
        let per_thread = 10_000;

        scope(|scope| {
            for t in 0..threads {
                let q = &q;
                scope.spawn(move || {
                    for i in 0..per_thread {
                        q.push(t * per_thread + i);
                    }
                });
            }
        });

        let mut count = 0;
        while q.try_pop().is_some() {
            count += 1;
        }
        assert_eq!(count, threads * per_thread);
        assert!(q.is_empty());
    }

    #[test]
    fn concurrent_push_sum_is_preserved() {
        let q = Queue::new();
        let threads = 8;
        let per_thread: i64 = 10_000;

        scope(|scope| {
            for t in 0..threads {
                let q = &q;
                scope.spawn(move || {
                    let start = t * per_thread;
                    for i in start..start + per_thread {
                        q.push(i);
                    }
                });
            }
        });

        let total = threads * per_thread;
        let expected_sum: i64 = (0..total).sum();
        let mut actual_sum: i64 = 0;
        while let Some(val) = q.try_pop() {
            actual_sum += val;
        }
        assert_eq!(actual_sum, expected_sum);
        assert!(q.is_empty());
    }

    #[test]
    fn concurrent_mpmc_pop_count_equals_push_count() {
        let q = Queue::new();
        let producers = 4;
        let consumers = 4;
        let per_producer = 10_000;
        let total = producers * per_producer;
        let pop_count = AtomicUsize::new(0);

        scope(|scope| {
            for t in 0..producers {
                let q = &q;
                scope.spawn(move || {
                    let start = t * per_producer;
                    for i in start..start + per_producer {
                        q.push(i);
                    }
                });
            }

            for _ in 0..consumers {
                let q = &q;
                let pop_count = &pop_count;
                scope.spawn(move || {
                    while pop_count.load(Ordering::Relaxed) < total {
                        if q.try_pop().is_some() {
                            pop_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        assert_eq!(pop_count.load(Ordering::Relaxed), total);
        assert!(q.is_empty());
    }

    #[test]
    fn concurrent_spsc_try_pop_receives_all_elements_in_fifo_order() {
        let q: Queue<i64> = Queue::new();
        assert!(q.is_empty());

        scope(|scope| {
            scope.spawn(|| {
                let mut next = 0;

                while next < CONC_COUNT {
                    if let Some(elem) = q.try_pop() {
                        assert_eq!(elem, next);
                        next += 1;
                    }
                }
            });

            for i in 0..CONC_COUNT {
                q.push(i)
            }
        });
    }

    #[test]
    fn concurrent_spmc_each_consumer_observes_monotonically_increasing_elements() {
        fn recv(q: &Queue<i64>) {
            let mut cur = -1;
            for _ in 0..CONC_COUNT {
                if let Some(elem) = q.try_pop() {
                    assert!(elem > cur);
                    cur = elem;

                    if cur == CONC_COUNT - 1 {
                        break;
                    }
                }
            }
        }

        let q: Queue<i64> = Queue::new();
        assert!(q.is_empty());
        scope(|scope| {
            for _ in 0..3 {
                scope.spawn(|| recv(&q));
            }

            scope.spawn(|| {
                for i in 0..CONC_COUNT {
                    q.push(i);
                }
            });
        });
    }

    #[test]
    fn concurrent_mpmc_consumers_observe_sorted_left_and_right_elements() {
        enum LR {
            Left(i64),
            Right(i64),
        }

        let q: Queue<LR> = Queue::new();
        assert!(q.is_empty());

        scope(|scope| {
            scope.spawn(|| {
                for i in 0..CONC_COUNT {
                    q.push(LR::Left(i))
                }
            });
            scope.spawn(|| {
                for i in 0..CONC_COUNT {
                    q.push(LR::Right(i))
                }
            });
            for _ in 0..2 {
                scope.spawn(|| {
                    let mut vl = vec![];
                    let mut vr = vec![];
                    for _ in 0..CONC_COUNT {
                        match q.try_pop() {
                            Some(LR::Left(x)) => vl.push(x),
                            Some(LR::Right(x)) => vr.push(x),
                            _ => {}
                        }
                    }

                    let mut vl2 = vl.clone();
                    let mut vr2 = vr.clone();
                    vl2.sort();
                    vr2.sort();

                    assert_eq!(vl, vl2);
                    assert_eq!(vr, vr2);
                });
            }
        });
    }

    #[test]
    fn concurrent_spsc_blocking_pop_matches_all_elements_and_empties() {
        let q: Queue<i64> = Queue::new();

        scope(|scope| {
            scope.spawn(|| {
                let mut next = 0;
                while next < CONC_COUNT {
                    assert_eq!(q.pop(), next);
                    next += 1;
                }
            });

            for i in 0..CONC_COUNT {
                q.push(i)
            }
        });
        assert!(q.is_empty());
    }
}

