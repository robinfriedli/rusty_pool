# rusty_pool

Simple self growing / shrinking `ThreadPool` implementation based on crossbeam's
multi-producer multi-consumer channels.

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