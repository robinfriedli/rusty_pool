## [0.6.0] - 2021-05-02

  * replace Atomic64 with AtomicUsize for worker count bookkeeping
    * refactor worker count bookkeeping arithmetics to work based on the
      size of usize on a given platform instead of requiring 64 bits
    * enables using the crate on non 64 bit platforms

## [0.5.1] - 2021-02-27

  * Replace compare_and_swap with compare_exchange_weak.
    * compare_and_swap has been deprecated as of rust 1.50.0

## [0.5.0] - 2021-01-30

  * Instead of incrementing the worker total and using the returned
    previous value to check if the worker is still required and then
    decrementing the counter again if that's not the case, use a compare
    and swap mechanism implemented by WorkerCountData::try_increment_worker_total
    to increment the total count if the current workerCountData matches
    the expected value and retry until it succeeds or until the observed
    value reaches the provided maximum value (either the core pool size
    when attempting to create a core worker, else the max pool size),
    at which point the pool proceeds to creating a non-core worker or
    to sending the task to the channel.
    * This only requires one atomic operation per retry instead of 2 (if
      contention is high it may lead to more retries as increments could
      not fail before but retries are much cheaper as it does not have to
      decrement the counter and recursively call the method) and
      guarantees that the total worker count is always accurate.
  * Use Ordering::Relaxed instead of SeqCst as the relaxed ordering suffices when incrementing a counter.
  * ThreadPool::default: Handle possible u32 overflow when calculating max_size.
  * Update dependencies.
  * Use twice the core count instead of 4 times the core count as default max_size.
  * Worker::mark_idle_and_notify_joiners_if_no_work: Check whether old_total_count equals old_idle_count + 1 before
    checking whether the receiver is empty as it is the cheaper operation.

## [0.4.3] - 2020-08-11

  * _do_join(): recheck whether the pool has become idle after acquiring the lock

## [0.4.2] - 2020-06-30

  * Builder: make sure core size is lower or equal to max size if max size was specified explicitly but core size wasn't.
    * build() panicked if max size was specified to be lower than default core size.
    * Set the core size to the specified max size if lower than the number of CPUs.
    * Handle possible u32 overflow when calculating default max size based on core size.
  * Remove unneeded `Sync` trait bounds.
  * Remove unneeded boxing.
  * Add additional tests.

## [0.4.1] - 2020-06-02

  * Add `spawn_await()` and `try_spawn_await()` as an extension of `spawn()` that creates a new top-level future that awaits
    the provided future and sends the result to a `JoinHandle`. This enables awaiting the final result outside of an async
    context like `complete()` while still polling the future lazily instead of eagerly blocking the worker until the future
    is done.
  * Ignore dropped oneshot receiver for `try_evaluate()`.
    * It is perfectly valid that the caller does not care about the result and drops the `JoinHandle` before
      the result has been completed. In that case attempting to send the result should not panic.

## [0.4.0] - 2020-06-02

  * Add async support
    * Add "async" feature that enables using the rusty_pool as a fully featured futures executor that may poll features
      and handle awakened futures.
    * Add "complete" function to simply block a worker until a future is polled to completion.
  * Add `JoinHandle` to enable awaiting a task's completion.
    * The `JoinHandle` holds the receiving end of a oneshot channel that receives the result of the task or a cancellation
      error if the task panics. The `await_complete()` and `try_await_complete()` functions can be used to block the calling
      thread until the task has completed or failed. The `JoinHandle` may also be sent to the `ThreadPool` to create a task
      that awaits the completion of the other task and then performs work using the result.
  * Add Builder struct to create a new `ThreadPool` more conveniently.
  * Name the `ThreadPool` and use the name as prefix for each worker thread's name.
  * Create `Task` trait representing everything that can be executed by the `ThreadPool`.
    * Add an implementation for any `FnOnce` closure that returns a thread-safe result
    * Add an implementation for `AsyncTask` representing a future that may be polled and awakened if the "async" feature
      is enabled.
  * Implement `Default` for `ThreadPool`.
  * Implement `Clone` for `ThreadPool`.
    * All clones will use the same channel and the same worker pool, meaning shutting down and dropping one clone will
      not cause the channel to break and other clones will still be intact. This also means that `shutdown_join()` will
      behave the same as calling `join()` on a live `ThreadPool` while only dropping said clone, meaning tasks submitted
      to other clones after `shutdown()` is called on this clone are joined as well.
    * `AsyncTask` instances hold a clone of the `ThreadPool` they were created for used to re-submit themselves when
      being awakened. This means the channel stays alive as long as there are pending `AsyncTask`s.
  * Update readme and documentation and add doctests.
  * Cleanup worker and channel data by putting them into separate structs.
    * Only 2 Arcs needed now by wrapping the entire `ChannelData` and `WorkerData` structs into an Arc.

## [0.3.2] - 2020-05-03

  * Add proper handling for panicking threads using a `Sentinel` that implements Drop and manages handling post-execution steps for workers that panicked while executing a
    submitted task and clones and replaces the worker.
  * Improve joins by also checking whether the `Receiver` is empty before fast-returning to eliminate a race condition where if a thread calls join in a brief window
    between one worker finishing a task and the next worker beginning a task.

## [0.3.1] - 2020-05-02

  * No longer wrap crossbeam Receivers inside an Arc because they already can be cloned and sent to other threads.

## [0.3.0] - 2020-05-01

  * Removed `Result` from `ThreadPool::execute()` and added `ThreadPool::try_execute()` instead.
    * Most often the user does not want to handle the result and prefers a panic in the very rare case the function returns an error (should not be possible with safe code).

## [0.2.0] - 2020-05-01

  * Added `shutdown_join()` and `shutdown_join_timeout(Duration)` to block the current thread until work in the pool is finished when shutting down the pool.
  * Improved bookkeeping of the total and idle worker counters by putting both values into one `AtomicU64`, which enables updating both counters in one single atomic operation.
    * this is part of a series of otherwise small changes to make joining the threadpool fully reliable and eliminating races where a thread could join the pool, see
      that the idle worker counter is lower than the total worker counter and assume that there is work to be joined when in fact the read was performed just before
      the idle worker counter was incremented, leading the thread to wait for work to be completed when all threads are idle. This should never happen now because
      an increment to both counter is one single atomic operation and the idle worker counter is incremented immediately after a worker finishes a task and before
      joining threads are notified. Because even if a thread would join exactly after the worker finished but before the idle worker count is incremented back to
      being equal to the total worker count it will get notified by the worker immediately after.
  * Improved Worker creation.
    * The workers now only spawn a thread once the re-check passes successfully, meaning the new worker is committed. The new worker is now started explicitly
      when the re_check succeeds and receives it's first task via the `start()` function directly. The `create_worker()` function now returns a Result where
      the Err variant contains the submitted task in case the re-check fails and the new worker is abandoned in which case the task will be sent to the main
      multi consumer channel instead.
  * If the re-recheck fails when creating new core worker try creating a non-core thread instead.
  * Fix workers notifying joining threads when the channel is empty but there's still threads executing work.
