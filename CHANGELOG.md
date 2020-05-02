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