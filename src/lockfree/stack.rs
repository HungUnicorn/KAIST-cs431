use core::mem::{self, MaybeUninit};
use core::ptr;
use core::sync::atomic::Ordering::*;

use crossbeam_epoch::{Atomic, Owned, Shared};

/// Treiber's lock-free stack.
///
/// Usable with any number of producers and consumers.
#[derive(Debug)]
pub struct Stack<T> {
    head: Atomic<Node<T>>,
}

#[derive(Debug)]
struct Node<T> {
    // MaybeUninit as the data may be taken out of the node.
    // TODO: fix the slides to sync with this.
    data: MaybeUninit<T>,
    next: *const Node<T>,
}

// Any particular `T` should never be accessed concurrently, so no need for `Sync`.
unsafe impl<T: Send> Send for Stack<T> {}
unsafe impl<T: Send> Sync for Stack<T> {}

impl<T> Default for Stack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Stack<T> {
    /// Creates a new, empty stack.
    pub fn new() -> Stack<T> {
        Self {
            head: Atomic::null(),
        }
    }

    /// Pushes a value on top of the stack.
    pub fn push(&self, t: T) {
        let mut node = Owned::new(Node {
            data: MaybeUninit::new(t),
            next: ptr::null(),
        });

        // SAFETY: We don't dereference any pointers obtained from this guard.
        let guard = unsafe { crossbeam_epoch::unprotected() };

        let mut head = self.head.load(Relaxed, guard);
        loop {
            node.next = head.as_raw();

            match self
                .head
                .compare_exchange(head, node, Release, Relaxed, guard)
            {
                Ok(_) => break,
                Err(e) => {
                    head = e.current;
                    node = e.new;
                }
            }
        }
    }

    /// Attempts to pop the top element from the stack.
    ///
    /// Returns `None` if the stack is empty.
    pub fn pop(&self) -> Option<T> {
        let mut guard = crossbeam_epoch::pin();

        loop {
            let head = self.head.load(Acquire, &guard);
            let h = unsafe { head.as_ref() }?;
            let next = Shared::from(h.next);

            if self
                .head
                .compare_exchange(head, next, Relaxed, Relaxed, &guard)
                .is_ok()
            {
                // Since the above `compare_exchange()` succeeded, `head` is detached from
                // `self` so is unreachable from other threads.

                // SAFETY: We are returning ownership of `data` in `head` by making a copy of it via
                // `assume_init_read()`. This is safe as no other thread has access to `data` after
                // `head` is unreachable, so the ownership of `data` in `head` will never be used
                // again.
                let result = unsafe { h.data.assume_init_read() };

                // SAFETY: `head` is unreachable, and we no longer access `head`.
                unsafe { guard.defer_destroy(head) };

                return Some(result);
            }

            // Repin to ensure the global epoch can make progress.
            guard.repin();
        }
    }

    /// Returns `true` if the stack is empty.
    pub fn is_empty(&self) -> bool {
        let guard = crossbeam_epoch::pin();
        self.head.load(Acquire, &guard).is_null()
    }
}

impl<T> Drop for Stack<T> {
    fn drop(&mut self) {
        let mut o_curr = mem::take(&mut self.head);

        // SAFETY: All non-null nodes made were valid, and we have unique ownership via `&mut self`.
        while let Some(curr) = unsafe { o_curr.try_into_owned() }.map(Owned::into_box) {
            drop(unsafe { curr.data.assume_init() });
            o_curr = curr.next.into();
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread::scope;

    use super::*;

    #[test]
    fn pop_empty_returns_none() {
        let stack: Stack<i32> = Stack::new();
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn is_empty_on_new() {
        let stack: Stack<i32> = Stack::new();
        assert!(stack.is_empty());
    }

    #[test]
    fn push_makes_non_empty() {
        let stack = Stack::new();
        stack.push(42);
        assert!(!stack.is_empty());
    }

    #[test]
    fn push_pop_single_element() {
        let stack = Stack::new();
        stack.push(42);
        assert_eq!(stack.pop(), Some(42));
        assert!(stack.is_empty());
    }

    #[test]
    fn push_pop_multiple() {
        let stack = Stack::new();
        stack.push(1);
        stack.push(2);
        stack.push(3);

        assert_eq!(stack.pop(), Some(3));
        assert_eq!(stack.pop(), Some(2));
        assert_eq!(stack.pop(), Some(1));
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn lifo_order() {
        let stack = Stack::new();
        for i in 0..10 {
            stack.push(i);
        }
        for i in (0..10).rev() {
            assert_eq!(stack.pop(), Some(i));
        }
        assert!(stack.is_empty());
    }

    #[test]
    fn interleaved_push_pop() {
        let stack = Stack::new();
        stack.push(1);
        stack.push(2);
        assert_eq!(stack.pop(), Some(2));

        stack.push(3);
        assert_eq!(stack.pop(), Some(3));
        assert_eq!(stack.pop(), Some(1));
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn drop_with_remaining_elements() {
        let stack = Stack::new();
        for i in 0..100 {
            stack.push(i);
        }
    }

    #[test]
    fn drop_calls_for_all_elements() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropCounter;
        impl Drop for DropCounter {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }

        DROP_COUNT.store(0, Ordering::Relaxed);
        {
            let stack = Stack::new();
            for _ in 0..50 {
                stack.push(DropCounter);
            }
            for _ in 0..10 {
                stack.pop();
            }
        }
        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 50);
    }

    #[test]
    fn concurrent_balanced_push_pop_is_empty() {
        let stack = Stack::new();

        scope(|scope| {
            for _ in 0..10 {
                scope.spawn(|| {
                    for i in 0..10_000 {
                        stack.push(i);
                        assert!(stack.pop().is_some());
                    }
                });
            }
        });

        assert!(stack.is_empty());
    }

    #[test]
    fn concurrent_push_then_pop_count_matches() {
        let stack = Stack::new();
        let threads = 8;
        let per_thread = 10_000;

        scope(|scope| {
            for t in 0..threads {
                let stack = &stack;
                scope.spawn(move || {
                    for i in 0..per_thread {
                        stack.push(t * per_thread + i);
                    }
                });
            }
        });

        let mut count = 0;
        while stack.pop().is_some() {
            count += 1;
        }
        assert_eq!(count, threads * per_thread);
    }

    #[test]
    fn concurrent_push_sum_is_preserved() {
        let stack = Stack::new();
        let threads = 8;
        let per_thread: i64 = 10_000;

        scope(|scope| {
            for t in 0..threads {
                let stack = &stack;
                scope.spawn(move || {
                    let start = t * per_thread;
                    for i in start..start + per_thread {
                        stack.push(i);
                    }
                });
            }
        });

        let total = threads * per_thread;
        let expected_sum: i64 = (0..total).sum();
        let mut actual_sum: i64 = 0;
        while let Some(val) = stack.pop() {
            actual_sum += val;
        }
        assert_eq!(actual_sum, expected_sum);
    }

    #[test]
    fn concurrent_mpmc_pop_count_equals_push_count() {
        let stack = Stack::new();
        let producers = 4;
        let consumers = 4;
        let per_producer: usize = 10_000;
        let total = producers * per_producer;
        let pop_count = AtomicUsize::new(0);

        scope(|scope| {
            for p in 0..producers {
                let stack = &stack;
                scope.spawn(move || {
                    for i in 0..per_producer {
                        stack.push(p * per_producer + i);
                    }
                });
            }

            for _ in 0..consumers {
                scope.spawn(|| {
                    while pop_count.load(Ordering::Relaxed) < total {
                        if stack.pop().is_some() {
                            pop_count.fetch_add(1, Ordering::Relaxed);
                        }
                        std::hint::spin_loop();
                    }
                });
            }
        });

        assert_eq!(pop_count.load(Ordering::Relaxed), total);
        assert!(stack.is_empty());
    }
}
