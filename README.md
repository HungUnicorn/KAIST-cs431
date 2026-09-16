# Concurrent Programming in Rust

Based on the rigorous curriculum of [KAIST CS431 (Concurrent Programming)](https://github.com/kaist-cp/cs431), this repository demonstrates hands-on expertise in multithreading, hardware memory models, and concurrent memory management.

## Learning Schedule

| Week | Focus Area | Core Topics & Implementations |
| :--- | :--- | :--- |
| **Week 1** | Foundations of Concurrency | Multithreading review, race conditions; Parallel TCP Web Server (`hello_server`) |
| **Week 2** | Systems Rust & Unsafe Memory | Safe vs. unsafe Rust, memory layout, raw pointers; Doubly Linked List (`linked_list.rs`) |
| **Week 3** | Lock-Based Concurrency (API) | Mutual exclusion, synchronization primitives, concurrent cache & thread pools |
| **Week 4** | Lock Implementation 1 | SpinLock, TicketLock, cache-line bouncing, MCS & CLH queue locks (`src/lock`) |
| **Week 5** | Lock Implementation 2 | Reader-writer synchronization, SeqLock, optimistic reads & validation |
| **Week 6** | Lock-Based Applications | Hand-over-hand lock coupling, fine-grained concurrent linked lists (`list_set`) |
| **Week 7** | Behavior-Oriented Concurrency (API) | Cowns, asynchronous message passing, `when!` clause (`boc.rs`) |
| **Week 8** | Midterm Milestone | Consolidation & review of lock-based concurrency and actor systems |
| **Week 9** | Lock-Free Concurrency (Concepts) | Hardware memory models, atomic orderings (`SeqCst`, `AcqRel`, `Relaxed`), Atomic Ref Counting (`arc.rs`) |
| **Week 10** | Lock-Free Data Structures 1 | Treiber Stack, Elimination Backoff Stack (`elim_stack`), Michael-Scott Queue |
| **Week 11** | Lock-Free Data Structures 2 | Harris lock-free linked list, atomic marked pointers, logical vs. physical deletion |
| **Week 12** | Lock-Free Data Structures 3 | Lock-free extensible hash table (Split-Ordered List), concurrent growable array (`hash_table`) |
| **Week 13** | Concurrency Specification | Correctness criteria, Linearizability, identifying linearization points |
| **Week 14** | Safe Memory Reclamation (SMR) | Hazard Pointers (`hazard_pointer`), Epoch-Based Reclamation (EBR), solving ABA problem |
| **Week 15** | Behavior-Oriented Concurrency (Engine) | Multi-core scheduler implementation for message-passing systems |
| **Week 16** | Final Synthesis | Full test suite execution and dynamic analysis with ThreadSanitizer & AddressSanitizer |

## Build & Test

### Prerequisites
- Rust 1.80+ (Nightly toolchain recommended for sanitizer instrumentation)
- Clang / LLVM (`llvm-symbolizer`)

### Running the Test Suite

```bash
# Run tests for core primitives
cargo test

# Run homework assignments & lock-free benchmarks
cd homework
cargo test
```

### Running with LLVM Sanitizers (Linux / macOS)

```bash
cd homework
source scripts/grade-utils.sh

# Run under AddressSanitizer (detect memory corruption & use-after-free)
cargo_asan test --test hazard_pointer

# Run under ThreadSanitizer (detect data races & memory ordering violations)
cargo_tsan test --test growable_array
```

---
