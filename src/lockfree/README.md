# Lockfree Data Structures Tests

Tests and documentation for lock-free data structures in `src/lockfree/`.

## Stack (`stack.rs`)

## Key Concepts in the Implementation

| Concept | Where in code |
|---|---|
| `Atomic<Node<T>>` as head pointer | `Stack::head` |
| CAS retry loop | `push()` loop, `pop()` loop |
| `Owned::new()` → allocate a node | `push()` |
| `crossbeam_epoch::pin()` → protect reads | `pop()`, `is_empty()` |
| `unsafe { crossbeam_epoch::unprotected() }` | `push()` — no deref needed |
| `MaybeUninit` + `assume_init_read()` | `pop()` — take data out without dropping the node yet |
| `guard.defer_destroy()` | `pop()` — schedule node deallocation after epoch advances |
| `guard.repin()` | `pop()` — let global epoch advance on CAS failure |
| `mem::take()` + `try_into_owned()` | `Drop` — walk list with exclusive access |

## Test Guide

### Single-threaded basics

| Test | What it verifies |
|---|---|
| `pop_empty_returns_none` | `Shared::as_ref()` returns `None` on null → `?` returns `None` |
| `is_empty_on_new` | `head.load()` is null on fresh stack |
| `push_makes_non_empty` | After push, head is no longer null |
| `push_pop_single_element` | Full push→pop cycle: `Owned::new`, CAS, `pin()`, `assume_init_read`, `defer_destroy` |
| `push_pop_multiple` | Multiple nodes linked via `node.next`, popped in reverse |

### LIFO ordering

| Test | What it verifies |
|---|---|
| `lifo_order` | Push sets `node.next = head`, CAS swaps head → newest always on top |
| `interleaved_push_pop` | Head pointer correctness across mixed push/pop sequences |

### Drop behavior

| Test | What it verifies |
|---|---|
| `drop_with_remaining_elements` | `Stack::drop()` walks list via `mem::take()` + `try_into_owned()`, no leak/double-free |
| `drop_calls_for_all_elements` | Atomic counter proves `Drop` called exactly 50 times: 10 via `pop` path (`assume_init_read`) + 40 via `Stack::drop` path (`assume_init`) |

### Concurrent correctness

| Test | What it verifies |
|---|---|
| `concurrent_balanced_push_pop_is_empty` | 10 threads × 10K push+pop — CAS retry loop handles contention, no crashes. Note: a thread may pop another thread's value |
| `concurrent_push_then_pop_count_matches` | 8 threads push, then single-threaded pop counts exactly 80K — CAS failure retries, no lost writes |
| `concurrent_push_sum_is_preserved` | Sum of all popped values matches expected — catches data corruption on CAS success |
| `concurrent_mpmc_pop_count_equals_push_count` | 4 producers + 4 consumers simultaneously — total pop count equals total push count |

---

## Queue (`queue.rs`)

Tests for the Michael & Scott lock-free concurrent queue (`queue.rs`).

### Key Concepts in the Implementation

| Concept | Where in code | Description |
|---|---|---|
| Sentinel Node | `Queue::new()` | Singly-linked list always maintains a dummy sentinel at the front. Sentinel holds uninitialized `MaybeUninit<T>`. |
| `CachePadded` | `head`, `tail` | Head and tail live on different cache lines to prevent false sharing under high producer/consumer contention. |
| Tail-Helping | `push()` loop | If `!tail.next.is_null()`, `tail` is lagging; the producer swings `tail` forward before attempting its own insertion. |
| Linearization: Enqueue | `push()` | Successful CAS on `tail_ref.next` from `null` to `new`. Swing of `tail` is helping / cleanup. |
| Linearization: Dequeue | `try_pop()` | Successful CAS on `head` from `head` to `next`. |
| Value Extraction | `try_pop()` | `next_ref.data.assume_init_read()` transfers ownership out; `next` becomes the new sentinel node. |
| Safe Drop | `Drop for Queue<T>` | Discards initial sentinel without reading its uninitialized data, then drops remaining nodes and their values. |

### Test Guide

#### Single-threaded basics & FIFO

| Test | What it verifies |
|---|---|
| `try_pop_empty_returns_none` | `try_pop()` returns `None` immediately when `head.next` is null. |
| `is_empty_on_new_returns_true` | `is_empty()` returns `true` on freshly initialized sentinel-only queue. |
| `push_makes_queue_non_empty` | After pushing one element, `is_empty()` returns `false`. |
| `single_push_try_pop_returns_element_and_becomes_empty` | Pushing 37 and popping returns `Some(37)` and leaves queue empty. |
| `two_pushes_try_pop_returns_fifo_order_and_becomes_empty` | Two elements emerge in FIFO order via `try_pop()`. |
| `single_push_blocking_pop_returns_element_and_becomes_empty` | Single push and blocking `pop()` returns value and leaves queue empty. |
| `two_pushes_blocking_pop_returns_elements_in_fifo_order` | Two pushes and blocking `pop()` returns values in FIFO order. |
| `push_multiple_makes_is_empty_false_and_allows_pop` | Multiple pushes keep queue non-empty and accessible. |
| `fifo_order_preserved` | 100 sequential pushes match 100 sequential pops in exact FIFO order. |
| `interleaved_push_pop_preserves_fifo_order` | Alternating push/pop operations maintain FIFO order across transitions. |
| `sequential_200_pushes_try_pop_all_in_fifo_order_and_becomes_empty` | 200 sequential elements drain via `try_pop()` in FIFO order. |
| `sequential_200_pushes_blocking_pop_all_in_fifo_order_and_becomes_empty` | 200 sequential elements drain via blocking `pop()` in FIFO order. |

#### Drop behavior

| Test | What it verifies |
|---|---|
| `drop_empty_queue_does_not_crash_or_read_uninit` | Sentinel-only queue dropped cleanly without reading uninitialized `MaybeUninit<T>`. |
| `drop_with_remaining_elements_does_not_crash` | Non-empty queue drops remaining nodes without leaks or double-frees. |
| `drop_calls_destructor_for_all_remaining_elements` | Proves all remaining elements have their destructor run exactly once on `Queue::drop` (and sentinel is not dropped as `T`). |

#### Concurrent correctness

| Test | What it verifies |
|---|---|
| `concurrent_push_then_pop_count_matches` | 8 concurrent producer threads push 80K items total; sequential drain asserts exactly 80K popped. |
| `concurrent_push_sum_is_preserved` | Concurrent multi-producer writes preserve full integer sum without data corruption or lost updates. |
| `concurrent_mpmc_pop_count_equals_push_count` | 4 producers + 4 consumers running concurrently consume all 40K items with zero loss or deadlock. |
| `concurrent_spsc_try_pop_receives_all_elements_in_fifo_order` | Single-producer single-consumer streaming 1M items with `try_pop()` preserves FIFO order. |
| `concurrent_spmc_each_consumer_observes_monotonically_increasing_elements` | 1 producer + 3 consumers: every consumer observes strictly increasing element values. |
| `concurrent_mpmc_consumers_observe_sorted_left_and_right_elements` | Two producers (tagged Left and Right) + two consumers verify per-stream ordering is preserved. |
| `concurrent_spsc_blocking_pop_matches_all_elements_and_empties` | Single-producer single-consumer streaming 1M items with blocking `pop()`. |

---

## List (`list.rs`)

Tests for the Harris lock-free sorted linked list (`list.rs`).

### Key Concepts in the Implementation

| Concept | Where in code | Description |
|---|---|---|
| Marked Pointer (Tag 1) | `Node::next.tag()` | Lowest bit indicates logical deletion. Prevents concurrent insertion after a deleted node. |
| Logical Deletion | `Cursor::delete()` | Atomically marks `curr.next` via `fetch_or(1, AcqRel)`. Represents the linearization point of deletion. |
| Physical Deletion | `find_*()`, `Cursor::delete()` | Bypasses marked node(s) by CASing `prev.next` to the next unmarked node, then schedules `defer_destroy`. |
| Cursor Traversal | `Cursor<'g, K, V>` | Holds `prev: &'g Atomic<Node>` and `curr: Shared<'g, Node>`. Required to splice nodes into or out of the singly linked chain. |
| Harris Strategy | `Cursor::find_harris()` | Batches consecutive marked nodes and unlinks the entire chain in a single CAS. |
| Harris-Michael Strategy | `Cursor::find_harris_michael()` | Unlinks marked nodes step-by-step (one node at a time) as soon as encountered during traversal. |
| Harris-Herlihy-Shavit Strategy | `Cursor::find_harris_herlihy_shavit()` | Read-only lookup traversal that does zero physical unlinking or cache-invalidating CAS writes. |

### Test Guide

Helpers in the test module:
- `linked_keys` walks the raw `next` pointers and returns **every physically linked key**, including marked (logically deleted) nodes. Comparing it before and after an operation shows exactly what the operation unlinked.
- `mark_without_unlinking` sets the mark bit with `fetch_or(1)` but skips the unlink CAS. This builds a list with marked nodes still linked in, a state that normally appears only when a deleter is preempted between marking and unlinking.
- `list_of` builds a list from a slice of keys (value = key).

#### Single-threaded basics & set semantics

| Test | What it verifies |
|---|---|
| `lookup_empty_returns_none` | `lookup` on an empty list returns `None` for all three strategies. |
| `insert_single_element_returns_true_and_lookup_matches` | Inserting one key returns `true` and the value can be looked up. |
| `duplicate_insert_returns_false_and_preserves_original_value` | Inserting an existing key returns `false` and keeps the original value. |
| `delete_nonexistent_key_returns_none` | Deleting a missing key returns `None`. |
| `delete_existing_key_returns_some_value_and_makes_subsequent_lookup_none` | Deleting returns `Some(value)`, and the key can no longer be found. |
| `multiple_unsorted_inserts_are_linked_in_sorted_order` | Keys inserted out of order end up physically linked in ascending order. |
| `interleaved_insert_delete_matches_expected_elements` | A mixed sequence of inserts and deletes leaves exactly the expected keys. |

#### Strategy comparisons

| Test | What it verifies |
|---|---|
| `harris_michael_insert_and_lookup_matches_inserted_value` | Harris-Michael insert and lookup work. |
| `harris_michael_delete_returns_value_and_makes_lookup_none` | Harris-Michael delete returns the value and removes the key. |
| `harris_herlihy_shavit_lookup_finds_present_key_and_returns_none_for_missing` | The read-only HHS lookup finds present keys and returns `None` for missing ones. |
| `all_three_find_strategies_agree_on_present_and_absent_keys` | All three strategies return the same result for every present and absent key. |

#### Logical vs. physical deletion

| Test | What it verifies |
|---|---|
| `lookup_returns_none_for_marked_but_still_linked_node` | A node whose key matches but whose `next` is marked counts as deleted, for all three strategies (`find_harris_herlihy_shavit` checks the mark after finding an equal key). |
| `harris_lookup_unlinks_chain_of_marked_nodes` | Marked nodes 20, 30, 40 stay linked until a Harris lookup walks past them; afterwards only 10 → 50 is linked. |
| `harris_michael_lookup_unlinks_chain_of_marked_nodes` | Same end state for Harris-Michael, which unlinks the marked nodes one at a time. |
| `harris_herlihy_shavit_lookup_leaves_marked_nodes_linked` | An HHS lookup skips marked nodes but never unlinks them: the read path does no writes. |

> [!NOTE]
> These tests mark nodes with `mark_without_unlinking` instead of calling `delete`. With one thread, `Cursor::delete` always succeeds at unlinking the node right after marking it, so `find` would never see a marked node and its cleanup code would go untested. Removing the cleanup from `find_harris` or `find_harris_michael` makes these tests fail.

#### Drop behavior

| Test | What it verifies |
|---|---|
| `drop_with_remaining_elements_does_not_crash` | `List::drop` frees every remaining node without crashing. |
| `drop_calls_destructor_for_all_remaining_values` | A drop counter proves `List::drop` drops each of the 50 remaining values exactly once. |

#### Concurrent correctness

| Test | What it verifies |
|---|---|
| `concurrent_disjoint_inserts_all_succeed_and_total_count_matches` | 8 threads insert disjoint key ranges; all 4,000 keys are present afterwards. |
| `concurrent_identical_inserts_only_one_succeeds_per_key` | 8 threads insert the same 200 keys; exactly 200 inserts succeed in total. |
| `concurrent_delete_of_same_key_succeeds_exactly_once` | 8 threads delete the same 200 keys; exactly 200 deletes succeed, because only one `fetch_or(1)` per node sees an unmarked pointer. |
| `concurrent_delete_evens_while_readers_always_find_odds` | 2 threads delete even keys while 4 readers (using all three strategies) repeatedly look up odd keys. No odd key is ever missing, so deleting a node never loses its neighbors. |
| `concurrent_harris_michael_insert_then_delete_own_keys_leaves_list_empty` | 8 threads insert and then delete their own keys with Harris-Michael; every delete returns its value and nothing stays linked. |
