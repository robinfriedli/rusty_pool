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
    * The workers now receive the task they were created for through the green-light single consumer channel that previously just sent a bool. The `create_worker()`
      function now returns a Result where the Err variant contains the submitted task in case the re-check fails and the new worker is abandoned in which case
      the task will be sent to the main multi consumer channel instead.
  * If the re-recheck fails when creating new core worker try creating a non-core thread instead.
  * Don't spawn a thread when creating a worker until after the re-check succeeds.
  * Fix workers notifying joining threads when the channel is empty but there's still threads executing work.