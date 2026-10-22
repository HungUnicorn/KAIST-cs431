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

