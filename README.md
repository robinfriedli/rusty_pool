# rusty_pool

Self growing / shrinking `ThreadPool` implementation based on crossbeam's
multi-producer multi-consumer channels that enables awaiting the result of a
task and offers async support.

This `ThreadPool` has two different pool sizes; a core pool size filled with
threads that live for as long as the channel and a max pool size which describes
the maximum amount of worker threads that may live at the same time.
Those additional non-core threads have a specific keep_alive time described when
creating the `ThreadPool` that defines how long such threads may be idle for
without receiving any work before giving up and terminating their work loop.

This `ThreadPool` does not spawn any threads until a task is submitted to it.
Then it will create a new thread for each task until the core pool size is full.
After that a new thread will only be created upon an `execute()` call if the
current pool is lower than the max pool size and there are no idle threads.

Functions like `evaluate()` and `complete()` return a `JoinHandle` that may be used
to await the result of a submitted task or future. JoinHandles may be sent to the
thread pool to create a task that blocks a worker thread until it receives the
result of the other task and then operates on the result. If the task panics the
`JoinHandle` receives a cancellation error. This is implemented using a futures
oneshot channel to communicate with the worker thread.

This `ThreadPool` may be used as a futures executor if the "async" feature is enabled,
which is the case by default. The "async" feature includes the `spawn()` and
`try_spawn()` functions which create a task that polls the future one by one and
creates a waker that re-submits the future to the pool when it can make progress.
Without the "async" feature, futures can simply be executed to completion using
the `complete` function, which simply blocks a worker thread until the future has
been polled to completion.

The "async" feature can be disabled if not need by adding the following to your
Cargo dependency:
```toml
[dependencies.rusty_pool]
default-features = false
version = "*"
```

When creating a new worker this `ThreadPool` tries to increment the worker count
using a compare-and-swap mechanism, if the increment fails because the total worker
count has been incremented to the specified limit (the core_size when trying to
create a core thread, else the max_size) by another thread, the pool tries to create
a non-core worker instead (if previously trying to create a core worker and no idle
worker exists) or sends the task to the channel instead. Panicking workers are always
cloned and replaced.

Locks are only used for the join functions to lock the `Condvar`, apart from that
this `ThreadPool` implementation fully relies on crossbeam and atomic operations.
This `ThreadPool` decides whether it is currently idle (and should fast-return
join attempts) by comparing the total worker count to the idle worker count, which
are two values stored in one `AtomicUsize` (both half the size of usize) making sure
that if both are updated they may be updated in a single atomic operation.

The thread pool and its crossbeam channel can be destroyed by using the shutdown
function, however that does not stop tasks that are already running but will
terminate the thread the next time it will try to fetch work from the channel.
The channel is only destroyed once all clones of the `ThreadPool` have been
shut down / dropped.

# Installation

To add rusty_pool to your project simply add the following Cargo dependency:
```toml
[dependencies]
rusty_pool = "0.7.0"
```

Or to exclude the "async" feature:
```toml
[dependencies.rusty_pool]
version = "0.7.0"
default-features = false
```

# Usage
Create a new `ThreadPool`:
```rust
use rusty_pool::Builder;
use rusty_pool::ThreadPool;
// Create default `ThreadPool` configuration with the number of CPUs as core pool size
let pool = ThreadPool::default();
// Create a `ThreadPool` with default naming:
use std::time::Duration;
let pool2 = ThreadPool::new(5, 50, Duration::from_secs(60));
// Create a `ThreadPool` with a custom name:
let pool3 = ThreadPool::new_named(String::from("my_pool"), 5, 50, Duration::from_secs(60));
// using the Builder struct:
let pool4 = Builder::new().core_size(5).max_size(50).build();
```

Submit a closure for execution in the `ThreadPool`:
```rust
use rusty_pool::ThreadPool;
use std::thread;
use std::time::Duration;
let pool = ThreadPool::default();
pool.execute(|| {
    thread::sleep(Duration::from_secs(5));
    print!("hello");
});
```

Submit a task and await the result:
```rust
use rusty_pool::ThreadPool;
use std::thread;
use std::time::Duration;
let pool = ThreadPool::default();
let handle = pool.evaluate(|| {
    thread::sleep(Duration::from_secs(5));
    return 4;
});
let result = handle.await_complete();
assert_eq!(result, 4);
```

Spawn futures using the `ThreadPool`:
```rust
async fn some_async_fn(x: i32, y: i32) -> i32 {
    x + y
}

async fn other_async_fn(x: i32, y: i32) -> i32 {
    x - y
}

use rusty_pool::ThreadPool;
let pool = ThreadPool::default();

// simply complete future by blocking a worker until the future has been completed
let handle = pool.complete(async {
    let a = some_async_fn(4, 6).await; // 10
    let b = some_async_fn(a, 3).await; // 13
    let c = other_async_fn(b, a).await; // 3
    some_async_fn(c, 5).await // 8
});
assert_eq!(handle.await_complete(), 8);

use std::sync::{Arc, atomic::{AtomicI32, Ordering}};

// spawn future and create waker that automatically re-submits itself to the threadpool if ready to make progress, this requires the "async" feature which is enabled by default
let count = Arc::new(AtomicI32::new(0));
let clone = count.clone();
pool.spawn(async move {
    let a = some_async_fn(3, 6).await; // 9
    let b = other_async_fn(a, 4).await; // 5
    let c = some_async_fn(b, 7).await; // 12
    clone.fetch_add(c, Ordering::SeqCst);
});
pool.join();
assert_eq!(count.load(Ordering::SeqCst), 12);
```

Join and shut down the `ThreadPool`:
```rust
use std::thread;
use std::time::Duration;
use rusty_pool::ThreadPool;
use std::sync::{Arc, atomic::{AtomicI32, Ordering}};

let pool = ThreadPool::default();
for _ in 0..10 {
    pool.execute(|| { thread::sleep(Duration::from_secs(10)) })
}
// wait for all threads to become idle, i.e. all tasks to be completed including tasks added by other threads after join() is called by this thread or for the timeout to be reached
pool.join_timeout(Duration::from_secs(5));

let count = Arc::new(AtomicI32::new(0));
for _ in 0..15 {
    let clone = count.clone();
    pool.execute(move || {
        thread::sleep(Duration::from_secs(5));
        clone.fetch_add(1, Ordering::SeqCst);
    });
}

// shut down and drop the only instance of this `ThreadPool` (no clones) causing the channel to be broken leading all workers to exit after completing their current work
// and wait for all workers to become idle, i.e. finish their work.
pool.shutdown_join();
assert_eq!(count.load(Ordering::SeqCst), 15);
```

# Performance
In terms of performance from the perspective of a thread submitting tasks to the pool, rusty_pool should offer better
performance than any pool using std::sync::mpsc (such as rust-threadpool) in most scenarios thanks to the great work of
the crossbeam team. In some cases with extreme contention rusty_pool might fall behind rust-threadpool, though the scenarios
where this has been found to be the case are hardly practical as they require to submit empty tasks in a loop and it depends
on the platform. macOS seems to perform particularly well in the tested scenario, presumably macOS has spent a lot of
effort optimising atomic operations as Swift's reference counting depends on it. Apparently this should be amplified on
Apple Silicon but rusty_pool has not been tested on that platform. The following tests were executed on a PC with an
AMD Ryzen 9 3950X for Linux and Windows and on a MacBook Pro 15" 2019 with an Intel i9-9880H for macOS.

### Test 1: No contention
All tasks are submitted by the same thread and the task lasts longer than the test, meaning all atomic operations
(reading and incrementing the worker counter) are performed by the main thread, since newly created workers do not
alter the counter until after they completed their initial task and increment the idle counter.

```rust
fn main() {
    let now = std::time::Instant::now();

    let pool = rusty_pool::Builder::new().core_size(10).max_size(10).build();
    //let pool = threadpool::ThreadPool::new(10);

    for _ in 0..10000000 {
        pool.execute(|| {
            thread::sleep(std::time::Duration::from_secs(1));
        });
    }

    let millis = now.elapsed().as_millis();
    println!("millis: {}", millis);
}
```

Results (in milliseconds, average value):

rusty_pool 0.5.1:

| Windows | MacOS | Linux |
|---------|-------|-------|
| 221.6   | 293.07| 183.73|

rusty_pool 0.5.0:

| Windows | MacOS | Linux |
|---------|-------|-------|
| 224.6   | 315.6 | 187.0 |

rust-threadpool 1.8.1:

| Windows | MacOS | Linux |
|---------|-------|-------|
| 476.4   | 743.4 | 354.3 |

rusty_pool 0.4.3:

| Windows | MacOS | Linux |
|---------|-------|-------|
| 237.5   | 318.1 | 181.3 |

### Test 2: Multiple producers
Next to the main thread there are 10 other threads submitting tasks to the pool. Unlike the previous test, the task no
longer lasts longer than the test, thus there not only is contention between the producers for the worker counter but also
between the worker threads updating the idle counter. This is a somewhat realistic albeit extreme example.

```rust
fn main() {
    let now = std::time::Instant::now();

    let pool = rusty_pool::Builder::new().core_size(10).max_size(10).build();
    //let pool = threadpool::ThreadPool::new(10);

    for _ in 0..10 {
        let pool = pool.clone();

        std::thread::spawn(move || {
            for _ in 0..10000000 {
                pool.execute(|| {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                });
            }
        });
    }

    for _ in 0..10000000 {
        pool.execute(|| {
            std::thread::sleep(std::time::Duration::from_secs(1));
        });
    }

    let millis = now.elapsed().as_millis();
    println!("millis: {}", millis);
}
```

Results (in milliseconds, average value):

rusty_pool 0.5.1:

| Windows* | MacOS  | Linux  |
|----------|--------|--------|
| 7692.4   | 3656.2 | 7514.53|

rusty_pool 0.5.0:

| Windows  | MacOS  | Linux  | Windows*|
|----------|--------|--------|---------|
| 6251.0   | 4417.7 | 7903.1 | 7774.67 |

rust-threadpool 1.8.1:

| Windows  | MacOS  | Linux  |
|----------|--------|--------|
| 10030.5  | 5810.5 | 9743.3 |

rusty_pool 0.4.3:

| Windows  | MacOS  | Linux  | Windows*|
|----------|--------|--------|---------|
| 6342.2   | 4444.6 | 7962.0 | 8564.93 |

&ast; When testing 0.5.1 the performance for Windows appeared to be considerably worse, so the results for previous versions
of rusty_pool were recalculated and also found to be worse than when originally recorded, probably due to external
influence (e.g. background task taking a lot of CPU time, though the test was retried with realtime priority with similar
results). The results for rust-threadpool 1.8.1 were not fully recalculated as they appeared to be similar to the last
recording.

### Test 3: Worst case
This test case highlights the aforementioned worst-case scenario for rusty_pool where the pool is spammed with empty
tasks. Since workers increment the idle counter after completing a task and the task is executed practically immediately,
the increment of the idle counter coincides with the next execute() call in the loop reading the counter. The higher the
number of workers the higher contention gets and the worse performance becomes.

```rust
fn main() {
    let now = std::time::Instant::now();

    let pool = rusty_pool::Builder::new().core_size(10).max_size(10).build();
    //let pool = threadpool::ThreadPool::new(10);

    for _ in 0..10000000 {
        pool.execute(|| {});
    }

    let millis = now.elapsed().as_millis();
    println!("millis: {}", millis);
}
```

rusty_pool 0.5.1:

| Windows  | MacOS  | Linux  |
|----------|--------|--------|
| 1967.93  | 698.8  | 2150.0 |

rusty_pool 0.5.0:

| Windows  | MacOS  | Linux  |
|----------|--------|--------|
| 1991.6   | 679.93 | 2175.1 |

rust-threadpool 1.8.1:

| Windows  | MacOS  | Linux  |
|----------|--------|--------|
| 980.33   | 1224.6 | 677.0  |

rusty_pool 0.4.3:

| Windows  | MacOS  | Linux  |
|----------|--------|--------|
| 2016.8   | 683.13 | 2175.1 |

Curiously, macOS heavily favours rusty_pool in this case while Windows and Linux favour rust-threadpool. However, this test
case should hardly occur in a real world scenario. In all other tested scenarios rusty_pool performs better when submitting
tasks, where macOS seems to gain a lead in cases where there is a lot of contention but falling behind in other cases,
possibly due to the weaker hardware of the specific device used for testing. Linux seems to perform best in cases with
little to no contention but performs the worst when contention is high.