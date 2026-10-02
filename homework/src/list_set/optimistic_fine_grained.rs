use std::cmp::Ordering::*;
use std::mem::{self, ManuallyDrop};
use std::sync::atomic::Ordering::*;

use crossbeam_epoch::{Atomic, Guard, Owned, Shared, pin};
use cs431::lock::seqlock::{ReadGuard, SeqLock};

use crate::ConcurrentSet;

#[derive(Debug)]
struct Node<T> {
    data: T,
    next: SeqLock<Atomic<Node<T>>>,
}

/// Concurrent sorted singly linked list using fine-grained optimistic locking.
#[derive(Debug)]
pub struct OptimisticFineGrainedListSet<T> {
    head: SeqLock<Atomic<Node<T>>>,
}

unsafe impl<T: Send> Send for OptimisticFineGrainedListSet<T> {}
unsafe impl<T: Sync> Sync for OptimisticFineGrainedListSet<T> {}

#[derive(Debug)]
struct Cursor<'g, T> {
    // Reference to the `next` field of previous node which points to the current node.
    prev: ReadGuard<'g, Atomic<Node<T>>>,
    curr: Shared<'g, Node<T>>,
}

impl<T> Node<T> {
    fn new(data: T, next: Shared<'_, Self>) -> Owned<Self> {
        Owned::new(Self {
            data,
            next: SeqLock::new(next.into()),
        })
    }
}

impl<'g, T: Ord> Cursor<'g, T> {
    /// Moves the cursor to the position of key in the sorted list.
    /// Returns whether the value was found.
    ///
    /// Return `Err(())` if the cursor cannot move.
    fn find(&mut self, key: &T, guard: &'g Guard) -> Result<bool, ()> {
        loop {
            let Some(curr_node) = (unsafe { self.curr.as_ref() }) else {
                return if self.prev.validate() {
                    Ok(false)
                } else {
                    Err(())
                };
            };

            match curr_node.data.cmp(key) {
                Equal => {
                    return if self.prev.validate() {
                        Ok(true)
                    } else {
                        Err(())
                    };
                }
                Greater => {
                    return if self.prev.validate() {
                        Ok(false)
                    } else {
                        Err(())
                    };
                }
                Less => {
                    let next_prev = unsafe { curr_node.next.read_lock() };
                    let next_curr = next_prev.load(Acquire, guard);

                    let old_prev = mem::replace(&mut self.prev, next_prev);
                    if !old_prev.finish() {
                        return Err(());
                    }

                    self.curr = next_curr;
                }
            }
        }
    }
}

impl<T> OptimisticFineGrainedListSet<T> {
    /// Creates a new list.
    pub fn new() -> Self {
        Self {
            head: SeqLock::new(Atomic::null()),
        }
    }

    fn head<'g>(&'g self, guard: &'g Guard) -> Cursor<'g, T> {
        let prev = unsafe { self.head.read_lock() };
        let curr = prev.load(Acquire, guard);
        Cursor { prev, curr }
    }
}

impl<T: Ord> OptimisticFineGrainedListSet<T> {
    fn find<'g>(&'g self, key: &T, guard: &'g Guard) -> Result<(bool, Cursor<'g, T>), ()> {
        let mut cursor = self.head(guard);
        match cursor.find(key, guard) {
            Ok(found) => Ok((found, cursor)),
            Err(()) => {
                let _ = cursor.prev.finish();
                Err(())
            }
        }
    }
}

impl<T: Ord> ConcurrentSet<T> for OptimisticFineGrainedListSet<T> {
    fn contains(&self, key: &T) -> bool {
        let guard = pin();
        loop {
            if let Ok((found, cursor)) = self.find(key, &guard) {
                if cursor.prev.finish() {
                    return found;
                }
            }
        }
    }

    fn insert(&self, key: T) -> bool {
        let guard = pin();
        loop {
            let Ok((found, cursor)) = self.find(&key, &guard) else {
                continue;
            };
            if found {
                if cursor.prev.finish() {
                    return false;
                }
            } else {
                let Ok(mut write_guard) = cursor.prev.upgrade() else {
                    continue;
                };
                let new_node = Node::new(key, cursor.curr);
                write_guard.store(new_node, Release);
                return true;
            }
        }
    }

    fn remove(&self, key: &T) -> bool {
        let guard = pin();
        loop {
            let Ok((found, cursor)) = self.find(key, &guard) else {
                continue;
            };

            if !found {
                if cursor.prev.finish() {
                    return false;
                }
                continue;
            }

            let Ok(mut prev_guard) = cursor.prev.upgrade() else {
                continue;
            };

            let curr_node = unsafe { cursor.curr.deref() };
            let mut curr_guard = curr_node.next.write_lock();

            let succ = curr_guard.load(Relaxed, &guard);
            prev_guard.store(succ, Release);

            unsafe {
                guard.defer_destroy(cursor.curr);
            }
            return true;
        }
    }
}

#[derive(Debug)]
pub struct Iter<'g, T> {
    // Can be dropped without validation, because the only way to use cursor.curr is next().
    cursor: ManuallyDrop<Cursor<'g, T>>,
    guard: &'g Guard,
}

impl<T> OptimisticFineGrainedListSet<T> {
    /// An iterator visiting all elements. `next()` returns `Some(Err(()))` when validation fails.
    /// In that case, the user must restart the iteration.
    pub fn iter<'g>(&'g self, guard: &'g Guard) -> Iter<'g, T> {
        Iter {
            cursor: ManuallyDrop::new(self.head(guard)),
            guard,
        }
    }
}

impl<'g, T> Iterator for Iter<'g, T> {
    type Item = Result<&'g T, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor.curr.is_null() {
            if !self.cursor.prev.validate() {
                Some(Err(()))
            } else {
                None
            }
        } else {
            let curr_node = unsafe { self.cursor.curr.deref() };
            let next_prev = unsafe { curr_node.next.read_lock() };
            let next_curr = next_prev.load(Acquire, self.guard);

            let old_prev = mem::replace(&mut self.cursor.prev, next_prev);
            if !old_prev.finish() {
                return Some(Err(()));
            }

            self.cursor.curr = next_curr;
            Some(Ok(&curr_node.data))
        }
    }
}

impl<T> Drop for OptimisticFineGrainedListSet<T> {
    fn drop(&mut self) {
        let guard = pin();
        let mut curr = self.head.get_mut().load(Relaxed, &guard);

        while !curr.is_null() {
            let mut node = unsafe { curr.into_owned() };
            let next = node.next.get_mut().load(Relaxed, &guard);
            drop(node);
            curr = next;
        }
    }
}

impl<T> Default for OptimisticFineGrainedListSet<T> {
    fn default() -> Self {
        Self::new()
    }
}
