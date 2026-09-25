# Lock Implementation Tests

Tests verifying the behavioral properties of 5 lock implementations in `src/lock/`.

## Running

```bash
cargo test --test lock
```

## Coverage Map

| Test | SpinLock | TicketLock | McsLock | ClhLock | McsParkingLock | What it proves |
| :--- | :---: | :---: | :---: | :---: | :---: | :--- |
| `fairness_fifo` | — | ✅ | ✅ | ✅ | ✅ | Queue/ticket locks service waiters in strict FIFO arrival order |
| `spinlock_unfair` | ✅ | — | — | — | — | SpinLock has no ordering guarantee (threads can barge) |
| `try_lock` | ✅ | — | — | — | — | Only SpinLock supports non-blocking `try_lock` (no queue to abandon) |
| `blocking_waiter` | — | — | — | — | ✅ | Waiter blocks and wakes correctly when lock is released |
| `stress_concurrent` | ✅ | ✅ | ✅ | ✅ | ✅ | Mutual exclusion holds under 8-thread contention |


## What tests cannot observe

McsLock vs ClhLock differ in their internal queue protocol, not in any externally observable behavior:

- **McsLock**: Thread spins on **its own** `node.locked`. Predecessor sets `successor.locked = false` on unlock.
- **ClhLock**: Thread spins on **predecessor's** `prev.locked`. Successor frees predecessor's node after acquiring.

This distinction requires reading the `lock()` and `unlock()` implementations side-by-side in
[`src/lock/mcslock.rs`](../../src/lock/mcslock.rs) and [`src/lock/clhlock.rs`](../../src/lock/clhlock.rs).
