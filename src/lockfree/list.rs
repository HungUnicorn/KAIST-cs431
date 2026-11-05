//! Lock-free singly linked list.

use core::cmp::Ordering::*;
use core::mem;
use core::sync::atomic::Ordering::*;

use crossbeam_epoch::{Atomic, Guard, Owned, Shared};

/// Linked list node.
// TODO: This node type is very brittle; what if some list creates a node, and uses it to add it to
// another, separate list? Also see the discussions at <https://github.com/kaist-cp/cs431/issues/957>.
// The public API surface is way too large.
#[derive(Debug)]
pub struct Node<K, V> {
    /// Mark: tag(), Tag: not needed
    next: Atomic<Node<K, V>>,
    key: K,
    value: V,
}

/// Sorted singly linked list.
///
/// Use-after-free will be caused when an unprotected guard is used, as the lifetime of returned
/// elements are linked to that of the guard in the same way a [`Shared`] is.
#[derive(Debug)]
pub struct List<K, V> {
    head: Atomic<Node<K, V>>,
}

// Unlike stack and queue, we need `K` and `V` to be `Sync` for the list to be `Sync`, as both `K`
// and `V` are accessed concurrently in `find` and `delete`, respectively.
unsafe impl<K: Sync, V: Sync> Sync for List<K, V> {}
unsafe impl<K: Send, V: Send> Send for List<K, V> {}

impl<K, V> Default for List<K, V>
where
    K: Ord,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Drop for List<K, V> {
    fn drop(&mut self) {
        let mut o_curr = mem::take(&mut self.head);
        // SAFETY: since we have `&mut self`, any references from `lookup()` must have finished.
        // Hence, we have sole ownership of `self` and its `Node`s.
        while let Some(curr) = unsafe { o_curr.try_into_owned() }.map(Owned::into_box) {
            o_curr = curr.next;
        }
    }
}

/// Linked list cursor.
#[derive(Debug)]
pub struct Cursor<'g, K, V> {
    prev: &'g Atomic<Node<K, V>>,
    // Tag of `curr` should always be zero so when `curr` is stored in a `prev`, we don't store a
    // marked pointer and cause cleanup to fail.
    curr: Shared<'g, Node<K, V>>,
}

// Manual implementation as deriving `Clone` leads to unnecessary trait bounds.
impl<K, V> Clone for Cursor<'_, K, V> {
    fn clone(&self) -> Self {
        Self {
            prev: self.prev,
            curr: self.curr,
        }
    }
}

impl<K, V> Node<K, V> {
    /// Creates a new node.
    pub fn new(key: K, value: V) -> Self {
        Self {
            next: Atomic::null(),
            key,
            value,
        }
    }

    /// Extracts the inner value.
    pub fn into_value(self) -> V {
        self.value
    }
}

impl<'g, K, V> Cursor<'g, K, V>
where
    K: Ord,
{
    /// Creates a cursor.
    pub fn new(prev: &'g Atomic<Node<K, V>>, curr: Shared<'g, Node<K, V>>) -> Self {
        Self {
            prev,
            curr: curr.with_tag(0),
        }
    }

    /// Returns the current node.
    pub fn curr(&self) -> Shared<'g, Node<K, V>> {
        self.curr
    }

    /// Clean up a chain of logically removed nodes in each traversal.
    #[inline]
    pub fn find_harris(&mut self, key: &K, guard: &'g Guard) -> Result<bool, ()> {
        // Finding phase
        // - cursor.curr: first unmarked node w/ key >= search key (4)
        // - cursor.prev: the ref of .next in previous unmarked node (1 -> 2)
        // 1 -> 2 -x-> 3 -x-> 4 -> 5 -> ∅  (search key: 4)
        let mut prev_next = self.curr;
        let found = loop {
            let Some(curr_node) = (unsafe { self.curr.as_ref() }) else {
                break false;
            };
            let next = curr_node.next.load(Acquire, guard);

            // - finding stage is done if cursor.curr advancement stops
            // - advance cursor.curr if (.next is marked) || (cursor.curr < key)
            // - stop cursor.curr if (not marked) && (cursor.curr >= key)
            // - advance cursor.prev if not marked

            if next.tag() != 0 {
                // We add a 0 tag here so that `self.curr`s tag is always 0.
                self.curr = next.with_tag(0);
                continue;
            }

            match curr_node.key.cmp(key) {
                Less => {
                    self.curr = next;
                    self.prev = &curr_node.next;
                    prev_next = next;
                }
                Equal => break true,
                Greater => break false,
            }
        };

        // If prev and curr WERE adjacent, no need to clean up
        if prev_next == self.curr {
            return Ok(found);
        }

        // cleanup marked nodes between prev and curr
        self.prev
            .compare_exchange(prev_next, self.curr, Release, Relaxed, guard)
            .map_err(|_| ())?;

        // defer_destroy from cursor.prev.load() to cursor.curr (exclusive)
        let mut node = prev_next;
        while node.with_tag(0) != self.curr {
            // SAFETY: All nodes in the unlinked chain are not null.
            //
            // NOTE: It may seem like this load could be non-atomic, but that would race with the
            // `fetch_or` done in `remove`.
            let next = unsafe { node.deref() }.next.load(Relaxed, guard);

            // SAFETY: we unlinked the chain with above CAS.
            unsafe { guard.defer_destroy(node) };
            node = next;
        }

        Ok(found)
    }

    /// Clean up a single logically removed node in each traversal.
    #[inline]
    pub fn find_harris_michael(&mut self, key: &K, guard: &'g Guard) -> Result<bool, ()> {
        loop {
            debug_assert_eq!(self.curr.tag(), 0);

            let Some(curr_node) = (unsafe { self.curr.as_ref() }) else {
                return Ok(false);
            };
            let mut next = curr_node.next.load(Acquire, guard);

            if next.tag() != 0 {
                next = next.with_tag(0);
                self.prev
                    .compare_exchange(self.curr, next, Release, Relaxed, guard)
                    .map_err(|_| ())?;
                unsafe { guard.defer_destroy(self.curr) };
                self.curr = next;
                continue;
            }

            match curr_node.key.cmp(key) {
                Less => {
                    self.prev = &curr_node.next;
                    self.curr = next;
                }
                Equal => return Ok(true),
                Greater => return Ok(false),
            }
        }
    }

    /// Doesn't preform any cleanup. Gotta go fast. Doesn't fail.
    #[inline]
    pub fn find_harris_herlihy_shavit(&mut self, key: &K, guard: &'g Guard) -> Result<bool, ()> {
        Ok(loop {
            let Some(curr_node) = (unsafe { self.curr.as_ref() }) else {
                break false;
            };
            match curr_node.key.cmp(key) {
                Less => {
                    // NOTE: unnecessary (this function is expected to be used only for `lookup`)
                    self.prev = &curr_node.next;
                    self.curr = curr_node.next.load(Acquire, guard);
                }
                Equal => break curr_node.next.load(Relaxed, guard).tag() == 0,
                Greater => break false,
            }
        })
    }

    /// Lookups the value at the current node.
    ///
    /// # Panics
    ///
    /// Panics if the current node is null.
    #[inline]
    pub fn lookup(&self) -> &'g V {
        &unsafe { self.curr.as_ref() }.unwrap().value
    }

    /// Inserts a value between the previous and current node.
    #[inline]
    pub fn insert(
        &mut self,
        mut node: Owned<Node<K, V>>,
        guard: &'g Guard,
    ) -> Result<(), Owned<Node<K, V>>> {
        node.next = self.curr.into();
        match self
            .prev
            .compare_exchange(self.curr, node, Release, Relaxed, guard)
        {
            Ok(node) => {
                self.curr = node;
                Ok(())
            }
            Err(e) => Err(e.new),
        }
    }

    /// Deletes the current node.
    ///
    /// # Panics
    ///
    /// Panics if the current node is null.
    #[inline]
    pub fn delete(&mut self, guard: &'g Guard) -> Result<&'g V, ()> {
        let curr_node = unsafe { self.curr.as_ref() }.unwrap();

        // Release: to release current view of the deleting thread on this mark.
        // Acquire: to ensure that if the latter CAS succeeds, then the thread that reads `next`
        // through `prev` will be safe.
        let next = curr_node.next.fetch_or(1, AcqRel, guard);
        if next.tag() == 1 {
            return Err(());
        }

        if self
            .prev
            .compare_exchange(self.curr, next, Release, Relaxed, guard)
            .is_ok()
        {
            // SAFETY: we are unlinker of curr. As the lifetime of the guard extends to the return
            // value of the function, later access of curr_node is ok.
            unsafe { guard.defer_destroy(self.curr) };
        }
        self.curr = next;

        Ok(&curr_node.value)
    }
}

impl<K, V> List<K, V>
where
    K: Ord,
{
    /// Creates a new list.
    pub fn new() -> Self {
        List {
            head: Atomic::null(),
        }
    }

    /// Creates the head cursor.
    #[inline]
    pub fn head<'g>(&'g self, guard: &'g Guard) -> Cursor<'g, K, V> {
        Cursor::new(&self.head, self.head.load(Acquire, guard))
    }

    /// Finds a key using the given find strategy.
    #[inline]
    fn find<'g, F>(&'g self, key: &K, find: &F, guard: &'g Guard) -> (bool, Cursor<'g, K, V>)
    where
        F: Fn(&mut Cursor<'g, K, V>, &K, &'g Guard) -> Result<bool, ()>,
    {
        loop {
            let mut cursor = self.head(guard);
            if let Ok(r) = find(&mut cursor, key, guard) {
                return (r, cursor);
            }
        }
    }

    #[inline]
    fn lookup<'g, F>(&'g self, key: &K, find: F, guard: &'g Guard) -> Option<&'g V>
    where
        F: Fn(&mut Cursor<'g, K, V>, &K, &'g Guard) -> Result<bool, ()>,
    {
        let (found, cursor) = self.find(key, &find, guard);
        if found {
            // `found` means current node cannot be null, so lookup won't panic.
            Some(cursor.lookup())
        } else {
            None
        }
    }

    #[inline]
    fn insert<'g, F>(&'g self, key: K, value: V, find: F, guard: &'g Guard) -> bool
    where
        F: Fn(&mut Cursor<'g, K, V>, &K, &'g Guard) -> Result<bool, ()>,
    {
        let mut node = Owned::new(Node::new(key, value));
        loop {
            let (found, mut cursor) = self.find(&node.key, &find, guard);
            if found {
                return false;
            }

            match cursor.insert(node, guard) {
                Ok(()) => return true,
                Err(n) => node = n,
            }
        }
    }

    #[inline]
    fn delete<'g, F>(&'g self, key: &K, find: F, guard: &'g Guard) -> Option<&'g V>
    where
        F: Fn(&mut Cursor<'g, K, V>, &K, &'g Guard) -> Result<bool, ()>,
    {
        loop {
            let (found, mut cursor) = self.find(key, &find, guard);
            if !found {
                return None;
            }

            if let Ok(value) = cursor.delete(guard) {
                return Some(value);
            }
        }
    }

    /// Lookups the value at `key` with the Harris strategy.
    pub fn harris_lookup<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.lookup(key, Cursor::find_harris, guard)
    }

    /// Insert the value with the Harris strategy.
    pub fn harris_insert<'g>(&'g self, key: K, value: V, guard: &'g Guard) -> bool {
        self.insert(key, value, Cursor::find_harris, guard)
    }

    /// Attempts to delete the value with the Harris strategy.
    pub fn harris_delete<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.delete(key, Cursor::find_harris, guard)
    }

    /// Lookups the value at `key` with the Harris-Michael strategy.
    pub fn harris_michael_lookup<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.lookup(key, Cursor::find_harris_michael, guard)
    }

    /// Insert a `key`-`value`` pair with the Harris-Michael strategy.
    pub fn harris_michael_insert(&self, key: K, value: V, guard: &Guard) -> bool {
        self.insert(key, value, Cursor::find_harris_michael, guard)
    }

    /// Delete the value at `key` with the Harris-Michael strategy.
    pub fn harris_michael_delete<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.delete(key, Cursor::find_harris_michael, guard)
    }

    /// Lookups the value at `key` with the Harris-Herlihy-Shavit strategy.
    pub fn harris_herlihy_shavit_lookup<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.lookup(key, Cursor::find_harris_herlihy_shavit, guard)
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread::scope;

    use crossbeam_epoch::pin;

    use super::*;

    fn linked_keys<V>(list: &List<i32, V>, guard: &Guard) -> Vec<i32> {
        let mut keys = Vec::new();
        let mut curr = list.head.load(Acquire, guard);
        while let Some(node) = unsafe { curr.as_ref() } {
            keys.push(node.key);
            curr = node.next.load(Acquire, guard);
        }
        keys
    }

    fn mark_without_unlinking<V>(list: &List<i32, V>, key: i32, guard: &Guard) {
        let mut curr = list.head.load(Acquire, guard);
        while let Some(node) = unsafe { curr.as_ref() } {
            if node.key == key {
                let _ = node.next.fetch_or(1, AcqRel, guard);
                return;
            }
            curr = node.next.load(Acquire, guard);
        }
        panic!("key {key} not found");
    }

    fn list_of(keys: &[i32], guard: &Guard) -> List<i32, i32> {
        let list = List::new();
        for &k in keys {
            assert!(list.harris_insert(k, k, guard));
        }
        list
    }

    #[test]
    fn lookup_empty_returns_none() {
        let list: List<i32, i32> = List::new();
        let guard = &pin();
        assert_eq!(list.harris_lookup(&10, guard), None);
        assert_eq!(list.harris_michael_lookup(&10, guard), None);
        assert_eq!(list.harris_herlihy_shavit_lookup(&10, guard), None);
    }

    #[test]
    fn insert_single_element_returns_true_and_lookup_matches() {
        let list: List<i32, &str> = List::new();
        let guard = &pin();
        assert!(list.harris_insert(10, "hello", guard));
        assert_eq!(list.harris_lookup(&10, guard), Some(&"hello"));
    }

    #[test]
    fn duplicate_insert_returns_false_and_preserves_original_value() {
        let list: List<i32, &str> = List::new();
        let guard = &pin();
        assert!(list.harris_insert(10, "first", guard));
        assert!(!list.harris_insert(10, "second", guard));
        assert_eq!(list.harris_lookup(&10, guard), Some(&"first"));
    }

    #[test]
    fn delete_nonexistent_key_returns_none() {
        let list: List<i32, i32> = List::new();
        let guard = &pin();
        assert_eq!(list.harris_delete(&42, guard), None);
    }

    #[test]
    fn delete_existing_key_returns_some_value_and_makes_subsequent_lookup_none() {
        let list: List<i32, &str> = List::new();
        let guard = &pin();
        assert!(list.harris_insert(10, "val", guard));
        assert_eq!(list.harris_delete(&10, guard), Some(&"val"));
        assert_eq!(list.harris_lookup(&10, guard), None);
    }

    #[test]
    fn multiple_unsorted_inserts_are_linked_in_sorted_order() {
        let guard = &pin();
        let list = list_of(&[50, 10, 40, 20, 30], guard);
        assert_eq!(linked_keys(&list, guard), vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn interleaved_insert_delete_matches_expected_elements() {
        let list: List<i32, i32> = List::new();
        let guard = &pin();

        assert!(list.harris_insert(1, 100, guard));
        assert!(list.harris_insert(3, 300, guard));
        assert!(list.harris_insert(2, 200, guard));

        assert_eq!(list.harris_delete(&2, guard), Some(&200));

        assert!(list.harris_insert(4, 400, guard));
        assert_eq!(list.harris_delete(&1, guard), Some(&100));

        assert_eq!(list.harris_lookup(&1, guard), None);
        assert_eq!(list.harris_lookup(&2, guard), None);
        assert_eq!(list.harris_lookup(&3, guard), Some(&300));
        assert_eq!(list.harris_lookup(&4, guard), Some(&400));
    }

    #[test]
    fn harris_michael_insert_and_lookup_matches_inserted_value() {
        let list: List<i32, &str> = List::new();
        let guard = &pin();
        assert!(list.harris_michael_insert(25, "michael", guard));
        assert_eq!(list.harris_michael_lookup(&25, guard), Some(&"michael"));
    }

    #[test]
    fn harris_michael_delete_returns_value_and_makes_lookup_none() {
        let list: List<i32, &str> = List::new();
        let guard = &pin();
        assert!(list.harris_michael_insert(25, "michael", guard));
        assert_eq!(list.harris_michael_delete(&25, guard), Some(&"michael"));
        assert_eq!(list.harris_michael_lookup(&25, guard), None);
    }

    #[test]
    fn harris_herlihy_shavit_lookup_finds_present_key_and_returns_none_for_missing() {
        let guard = &pin();
        let list = list_of(&[10, 20], guard);
        assert_eq!(list.harris_herlihy_shavit_lookup(&10, guard), Some(&10));
        assert_eq!(list.harris_herlihy_shavit_lookup(&20, guard), Some(&20));
        assert_eq!(list.harris_herlihy_shavit_lookup(&30, guard), None);
    }

    #[test]
    fn all_three_find_strategies_agree_on_present_and_absent_keys() {
        let list: List<i32, i32> = List::new();
        let guard = &pin();
        for i in (0..20).step_by(2) {
            assert!(list.harris_insert(i, i * 10, guard));
        }

        for i in 0..20 {
            let h = list.harris_lookup(&i, guard);
            let hm = list.harris_michael_lookup(&i, guard);
            let hhs = list.harris_herlihy_shavit_lookup(&i, guard);
            assert_eq!(h, hm);
            assert_eq!(hm, hhs);
        }
    }

    #[test]
    fn lookup_returns_none_for_marked_but_still_linked_node() {
        let guard = &pin();

        let list = list_of(&[10, 20, 30], guard);
        mark_without_unlinking(&list, 20, guard);
        assert_eq!(list.harris_herlihy_shavit_lookup(&20, guard), None);

        let list = list_of(&[10, 20, 30], guard);
        mark_without_unlinking(&list, 20, guard);
        assert_eq!(list.harris_michael_lookup(&20, guard), None);

        let list = list_of(&[10, 20, 30], guard);
        mark_without_unlinking(&list, 20, guard);
        assert_eq!(list.harris_lookup(&20, guard), None);
    }

    #[test]
    fn harris_lookup_unlinks_chain_of_marked_nodes() {
        let guard = &pin();
        let list = list_of(&[10, 20, 30, 40, 50], guard);
        mark_without_unlinking(&list, 20, guard);
        mark_without_unlinking(&list, 30, guard);
        mark_without_unlinking(&list, 40, guard);
        assert_eq!(linked_keys(&list, guard), vec![10, 20, 30, 40, 50]);

        assert_eq!(list.harris_lookup(&50, guard), Some(&50));

        assert_eq!(linked_keys(&list, guard), vec![10, 50]);
    }

    #[test]
    fn harris_michael_lookup_unlinks_chain_of_marked_nodes() {
        let guard = &pin();
        let list = list_of(&[10, 20, 30, 40, 50], guard);
        mark_without_unlinking(&list, 20, guard);
        mark_without_unlinking(&list, 30, guard);
        mark_without_unlinking(&list, 40, guard);
        assert_eq!(linked_keys(&list, guard), vec![10, 20, 30, 40, 50]);

        assert_eq!(list.harris_michael_lookup(&50, guard), Some(&50));

        assert_eq!(linked_keys(&list, guard), vec![10, 50]);
    }

    #[test]
    fn harris_herlihy_shavit_lookup_leaves_marked_nodes_linked() {
        let guard = &pin();
        let list = list_of(&[10, 20, 30, 40, 50], guard);
        mark_without_unlinking(&list, 20, guard);
        mark_without_unlinking(&list, 30, guard);

        assert_eq!(list.harris_herlihy_shavit_lookup(&50, guard), Some(&50));

        assert_eq!(linked_keys(&list, guard), vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn drop_with_remaining_elements_does_not_crash() {
        let guard = &pin();
        let list: List<i32, i32> = List::new();
        for i in 0..100 {
            list.harris_insert(i, i, guard);
        }
    }

    #[test]
    fn drop_calls_destructor_for_all_remaining_values() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropCounter;
        impl Drop for DropCounter {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }

        DROP_COUNT.store(0, Ordering::Relaxed);
        {
            let guard = &pin();
            let list = List::new();
            for i in 0..50 {
                assert!(list.harris_insert(i, DropCounter, guard));
            }
        }
        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 50);
    }

    #[test]
    fn concurrent_disjoint_inserts_all_succeed_and_total_count_matches() {
        let list: List<i32, i32> = List::new();
        let threads = 8;
        let per_thread = 500;

        scope(|scope| {
            for t in 0..threads {
                let list = &list;
                scope.spawn(move || {
                    let guard = &pin();
                    for i in 0..per_thread {
                        let key = t * per_thread + i;
                        assert!(list.harris_insert(key, key, guard));
                    }
                });
            }
        });

        let guard = &pin();
        let total = threads * per_thread;
        let found = (0..total)
            .filter(|i| list.harris_lookup(i, guard).is_some())
            .count();
        assert_eq!(found, total as usize);
    }

    #[test]
    fn concurrent_identical_inserts_only_one_succeeds_per_key() {
        let list: List<i32, i32> = List::new();
        let threads = 8;
        let num_keys = 200;
        let success_count = AtomicUsize::new(0);

        scope(|scope| {
            for _ in 0..threads {
                let list = &list;
                let success_count = &success_count;
                scope.spawn(move || {
                    let guard = &pin();
                    for key in 0..num_keys {
                        if list.harris_insert(key, key * 10, guard) {
                            success_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        assert_eq!(success_count.load(Ordering::Relaxed), num_keys as usize);

        let guard = &pin();
        for key in 0..num_keys {
            assert_eq!(list.harris_lookup(&key, guard), Some(&(key * 10)));
        }
    }

    #[test]
    fn concurrent_delete_of_same_key_succeeds_exactly_once() {
        let threads = 8;
        let num_keys = 200;
        let guard = &pin();
        let list = list_of(&(0..num_keys).collect::<Vec<_>>(), guard);
        let success_count = AtomicUsize::new(0);

        scope(|scope| {
            for _ in 0..threads {
                let list = &list;
                let success_count = &success_count;
                scope.spawn(move || {
                    let guard = &pin();
                    for key in 0..num_keys {
                        if list.harris_delete(&key, guard).is_some() {
                            success_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        assert_eq!(success_count.load(Ordering::Relaxed), num_keys as usize);
        assert_eq!(linked_keys(&list, guard), Vec::<i32>::new());
    }

    #[test]
    fn concurrent_delete_evens_while_readers_always_find_odds() {
        let total = 2000;
        let deleters = 2;
        let readers = 4;
        let guard = &pin();
        let list = list_of(&(0..total).collect::<Vec<_>>(), guard);
        let finished_deleters = AtomicUsize::new(0);
        let deleting_done = AtomicBool::new(false);

        scope(|scope| {
            for d in 0..deleters {
                let list = &list;
                let finished_deleters = &finished_deleters;
                let deleting_done = &deleting_done;
                scope.spawn(move || {
                    let guard = &pin();
                    for key in (0..total).filter(|k| k % 2 == 0 && (k / 2) % deleters == d) {
                        assert_eq!(list.harris_delete(&key, guard), Some(&key));
                    }
                    if finished_deleters.fetch_add(1, Ordering::AcqRel) + 1 == deleters as usize {
                        deleting_done.store(true, Ordering::Release);
                    }
                });
            }

            for r in 0..readers {
                let list = &list;
                let deleting_done = &deleting_done;
                scope.spawn(move || {
                    let guard = &pin();
                    loop {
                        let done = deleting_done.load(Ordering::Acquire);
                        for key in (1..total).step_by(2) {
                            let found = match r % 3 {
                                0 => list.harris_lookup(&key, guard),
                                1 => list.harris_michael_lookup(&key, guard),
                                _ => list.harris_herlihy_shavit_lookup(&key, guard),
                            };
                            assert_eq!(found, Some(&key));
                        }
                        if done {
                            break;
                        }
                    }
                });
            }
        });

        let expected: Vec<i32> = (1..total).step_by(2).collect();
        assert_eq!(linked_keys(&list, guard), expected);
    }

    #[test]
    fn concurrent_harris_michael_insert_then_delete_own_keys_leaves_list_empty() {
        let list: List<i32, i32> = List::new();
        let threads = 8;
        let per_thread = 500;

        scope(|scope| {
            for t in 0..threads {
                let list = &list;
                scope.spawn(move || {
                    let guard = &pin();
                    let keys = t * per_thread..(t + 1) * per_thread;
                    for key in keys.clone() {
                        assert!(list.harris_michael_insert(key, key, guard));
                    }
                    for key in keys {
                        assert_eq!(list.harris_michael_delete(&key, guard), Some(&key));
                    }
                });
            }
        });

        let guard = &pin();
        assert_eq!(linked_keys(&list, guard), Vec::<i32>::new());
    }
}
