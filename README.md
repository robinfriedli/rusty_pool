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

When creating a new worker this `ThreadPool` always re-checks whether the new worker
is still required before spawning a thread and passing it the submitted task in case
an idle thread has opened up in the meantime or another thread has already created
the worker. If the re-check failed for a core worker the pool will try creating a
new non-core worker before deciding no new worker is needed. Panicking workers are
always cloned and replaced.

Locks are only used for the join functions to lock the `Condvar`, apart from that
this `ThreadPool` implementation fully relies on crossbeam and atomic operations.
This `ThreadPool` decides whether it is currently idle (and should fast-return
join attempts) by comparing the total worker count to the idle worker count, which
are two `u32` values stored in one `AtomicU64` making sure that if both are updated
they may be updated in a single atomic operation.

The thread pool and its crossbeam channel can be destroyed by using the shutdown
function, however that does not stop tasks that are already running but will
terminate the thread the next time it will try to fetch work from the channel.

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

# Installation

To add rusty_pool to your project simply add the following Cargo dependency:
```toml
[dependencies]
rusty_pool = "0.4.1"
```

Or to exclude the "async" feature:
```toml
[dependencies.rusty_pool]
version = "0.4.1"
default-features = false
```