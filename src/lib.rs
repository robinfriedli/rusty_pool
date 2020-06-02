#[cfg(feature = "async")]
use futures::{
    future::BoxFuture,
    task::{waker_ref, ArcWake},
};
use futures_channel::oneshot;
use futures_executor::block_on;
use std::future::Future;
use std::option::Option;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, Condvar, Mutex,
};
#[cfg(feature = "async")]
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Trait to implement for all items that may be executed by the `ThreadPool`.
pub trait Task<R: Send + Sync>: Send + Sync {
    /// Execute this task and return its result.
    fn run(self) -> R;

    /// Transform this `Task` into a heap allocated `FnOnce` if possible.
    ///
    /// Used by [`ThreadPool::execute`](struct.ThreadPool.html#method.execute) to turn this `Task` into a `Job`
    /// directly without having to create an additional `Job` that calls this `Task`.
    fn as_fn(self) -> Option<Box<dyn FnOnce() -> R + Send + 'static>>;

    /// Return `true` if calling [`as_fn()`](trait.Task.html#method.as_fn) on this `Task` returns `Some`.
    fn is_fn(&self) -> bool;
}

/// Implement the `Task` trait for any FnOnce closure that returns a thread-safe result.
impl<R, F> Task<R> for F
where
    R: Send + Sync,
    F: FnOnce() -> R + Send + Sync + 'static,
{
    fn run(self) -> R {
        self()
    }

    fn as_fn(self) -> Option<Box<dyn FnOnce() -> R + Send + 'static>> {
        Some(Box::new(self))
    }

    fn is_fn(&self) -> bool {
        true
    }
}

/// Handle returned by [`ThreadPool::evaluate`](struct.ThreadPool.html#method.evaluate) and [`ThreadPool::complete`](struct.ThreadPool.html#method.complete)
/// that allows to block the current thread and wait for the result of a submitted task. The returned `JoinHandle` may also be sent to the [`ThreadPool`](struct.ThreadPool.html)
/// to create a task that blocks a worker thread until the task is completed and then does something with the result. This handle communicates with the worker thread
/// using a oneshot channel blocking the thread when [`try_await_complete()`](struct.JoinHandle.html#method.try_await_complete) is called until a message, i.e. the result of the
/// task, is received.
pub struct JoinHandle<T: Send + Sync> {
    receiver: oneshot::Receiver<T>,
}

impl<T: Send + Sync> JoinHandle<T> {
    /// Block the current thread until the result of the task is received.
    ///
    /// # Errors
    ///
    /// This function might return a `oneshot::Canceled` if the channel was broken
    /// before the result was received. This is generally the case if execution of
    /// the task panicked.
    pub fn try_await_complete(self) -> Result<T, oneshot::Canceled> {
        block_on(self.receiver)
    }

    /// Block the current thread until the result of the task is received.
    ///
    /// # Panics
    ///
    /// This function might panic if [`try_await_complete()`](struct.JoinHandle.html#method.try_await_complete) returns `oneshot::Canceled`.
    /// This is generally the case if execution of the task panicked and the sender was dropped before sending a result to the receiver.
    pub fn await_complete(self) -> T {
        self.try_await_complete()
            .expect("could not receive message because channel was cancelled")
    }
}

#[cfg(feature = "async")]
struct AsyncTask {
    future: Mutex<Option<BoxFuture<'static, ()>>>,
    pool: ThreadPool,
}

/// Implement `ArcWake` for `AsyncTask` by re-submitting the `AsyncTask` i.e. the `Future` to the pool.
#[cfg(feature = "async")]
impl ArcWake for AsyncTask {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let cloned_task = arc_self.clone();
        arc_self
            .pool
            .try_execute(cloned_task)
            .expect("failed to wake future because message could not be sent to pool");
    }
}

/// Implement the `Task` trait for `AsyncTask` in order to make it executable for the pool by
/// creating a waker and polling the future.
#[cfg(feature = "async")]
impl Task<()> for Arc<AsyncTask> {
    fn run(self) {
        let mut future_slot = self.future.lock().expect("failed to acquire mutex");
        if let Some(mut future) = future_slot.take() {
            let waker = waker_ref(&self);
            let context = &mut Context::from_waker(&*waker);
            if let Poll::Pending = future.as_mut().poll(context) {
                *future_slot = Some(future);
            }
        }
    }

    fn as_fn(self) -> Option<Box<dyn FnOnce() + Send + 'static>> {
        None
    }

    fn is_fn(&self) -> bool {
        false
    }
}

// assert that Send + Sync is implemented
trait ThreadSafe: Send + Sync {}

impl<R: Send + Sync> ThreadSafe for dyn Task<R> {}

impl<R: Send + Sync> ThreadSafe for JoinHandle<R> {}

impl ThreadSafe for ThreadPool {}

/// Self growing / shrinking `ThreadPool` implementation based on crossbeam's
/// multi-producer multi-consumer channels that enables awaiting the result of a
/// task and offers async support.
///
/// This `ThreadPool` has two different pool sizes; a core pool size filled with
/// threads that live for as long as the channel and a max pool size which describes
/// the maximum amount of worker threads that may live at the same time.
/// Those additional non-core threads have a specific keep_alive time described when
/// creating the `ThreadPool` that defines how long such threads may be idle for
/// without receiving any work before giving up and terminating their work loop.
///
/// This `ThreadPool` does not spawn any threads until a task is submitted to it.
/// Then it will create a new thread for each task until the core pool size is full.
/// After that a new thread will only be created upon an `execute()` call if the
/// current pool is lower than the max pool size and there are no idle threads.
///
/// Functions like `evaluate()` and `complete()` return a `JoinHandle` that may be used
/// to await the result of a submitted task or future. JoinHandles may be sent to the
/// thread pool to create a task that blocks a worker thread until it receives the
/// result of the other task and then operates on the result. If the task panics the
/// `JoinHandle` receives a cancellation error. This is implemented using a futures
/// oneshot channel to communicate with the worker thread.
///
/// This `ThreadPool` may be used as a futures executor if the "async" feature is enabled,
/// which is the case by default. The "async" feature includes the `spawn()` and
/// `try_spawn()` functions which create a task that polls the future one by one and
/// creates a waker that re-submits the future to the pool when it can make progress.
/// Without the "async" feature, futures can simply be executed to completion using
/// the `complete` function, which simply blocks a worker thread until the future has
/// been polled to completion.
///
/// The "async" feature can be disabled if not need by adding the following to your
/// Cargo dependency:
/// ```toml
/// [dependencies.rusty_pool]
/// default-features = false
/// version = "*"
/// ```
///
/// When creating a new worker this `ThreadPool` always re-checks whether the new worker
/// is still required before spawning a thread and passing it the submitted task in case
/// an idle thread has opened up in the meantime or another thread has already created
/// the worker. If the re-check failed for a core worker the pool will try creating a
/// new non-core worker before deciding no new worker is needed. Panicking workers are
/// always cloned and replaced.
///
/// Locks are only used for the join functions to lock the `Condvar`, apart from that
/// this `ThreadPool` implementation fully relies on crossbeam and atomic operations.
/// This `ThreadPool` decides whether it is currently idle (and should fast-return
/// join attempts) by comparing the total worker count to the idle worker count, which
/// are two `u32` values stored in one `AtomicU64` making sure that if both are updated
/// they may be updated in a single atomic operation.
///
/// The thread pool and its crossbeam channel can be destroyed by using the shutdown
/// function, however that does not stop tasks that are already running but will
/// terminate the thread the next time it will try to fetch work from the channel.
///
/// # Usage
/// Create a new `ThreadPool`:
/// ```rust
/// use rusty_pool::Builder;
/// use rusty_pool::ThreadPool;
/// // Create default `ThreadPool` configuration with the number of CPUs as core pool size
/// let pool = ThreadPool::default();
/// // Create a `ThreadPool` with default naming:
/// use std::time::Duration;
/// let pool2 = ThreadPool::new(5, 50, Duration::from_secs(60));
/// // Create a `ThreadPool` with a custom name:
/// let pool3 = ThreadPool::new_named(String::from("my_pool"), 5, 50, Duration::from_secs(60));
/// // using the Builder struct:
/// let pool4 = Builder::new().core_size(5).max_size(50).build();
/// ```
///
/// Submit a closure for execution in the `ThreadPool`:
/// ```rust
/// use rusty_pool::ThreadPool;
/// use std::thread;
/// use std::time::Duration;
/// let pool = ThreadPool::default();
/// pool.execute(|| {
///     thread::sleep(Duration::from_secs(5));
///     print!("hello");
/// });
/// ```
///
/// Submit a task and await the result:
/// ```rust
/// use rusty_pool::ThreadPool;
/// use std::thread;
/// use std::time::Duration;
/// let pool = ThreadPool::default();
/// let handle = pool.evaluate(|| {
///     thread::sleep(Duration::from_secs(5));
///     return 4;
/// });
/// let result = handle.await_complete();
/// assert_eq!(result, 4);
/// ```
///
/// Spawn futures using the `ThreadPool`:
/// ```rust
/// async fn some_async_fn(x: i32, y: i32) -> i32 {
///     x + y
/// }
///
/// async fn other_async_fn(x: i32, y: i32) -> i32 {
///     x - y
/// }
///
/// use rusty_pool::ThreadPool;
/// let pool = ThreadPool::default();
///
/// // simply complete future by blocking a worker until the future has been completed
/// let handle = pool.complete(async {
///     let a = some_async_fn(4, 6).await; // 10
///     let b = some_async_fn(a, 3).await; // 13
///     let c = other_async_fn(b, a).await; // 3
///     some_async_fn(c, 5).await // 8
/// });
/// assert_eq!(handle.await_complete(), 8);
///
/// use std::sync::{Arc, atomic::{AtomicI32, Ordering}};
///
/// // spawn future and create waker that automatically re-submits itself to the threadpool if ready to make progress, this requires the "async" feature which is enabled by default
/// let count = Arc::new(AtomicI32::new(0));
/// let clone = count.clone();
/// pool.spawn(async move {
///     let a = some_async_fn(3, 6).await; // 9
///     let b = other_async_fn(a, 4).await; // 5
///     let c = some_async_fn(b, 7).await; // 12
///     clone.fetch_add(c, Ordering::SeqCst);
/// });
/// pool.join();
/// assert_eq!(count.load(Ordering::SeqCst), 12);
/// ```
///
/// Join and shut down the `ThreadPool`:
/// ```rust
/// use std::thread;
/// use std::time::Duration;
/// use rusty_pool::ThreadPool;
/// use std::sync::{Arc, atomic::{AtomicI32, Ordering}};
///
/// let pool = ThreadPool::default();
/// for _ in 0..10 {
///     pool.execute(|| { thread::sleep(Duration::from_secs(10)) })
/// }
/// // wait for all threads to become idle, i.e. all tasks to be completed including tasks added by other threads after join() is called by this thread or for the timeout to be reached
/// pool.join_timeout(Duration::from_secs(5));
///
/// let count = Arc::new(AtomicI32::new(0));
/// for _ in 0..15 {
///     let clone = count.clone();
///     pool.execute(move || {
///         thread::sleep(Duration::from_secs(5));
///         clone.fetch_add(1, Ordering::SeqCst);
///     });
/// }
///
/// // shut down and drop the only instance of this `ThreadPool` (no clones) causing the channel to be broken leading all workers to exit after completing their current work
/// // and wait for all workers to become idle, i.e. finish their work.
/// pool.shutdown_join();
/// assert_eq!(count.load(Ordering::SeqCst), 15);
/// ```
#[derive(Clone)]
pub struct ThreadPool {
    core_size: u32,
    max_size: u32,
    keep_alive: Duration,
    channel_data: Arc<ChannelData>,
    worker_data: Arc<WorkerData>,
}

impl ThreadPool {
    /// Construct a new `ThreadPool` with the specified core pool size, max pool size
    /// and keep_alive time for non-core threads. This function does not spawn any
    /// threads. This `ThreadPool` will receive a default name in the following format:
    /// "rusty_pool_" + pool number.
    ///
    /// `core_size` specifies the amount of threads to keep alive for as long as
    /// the `ThreadPool` exists and its channel remains connected.
    ///
    /// `max_size` specifies the maximum number of worker threads that may exist
    /// at the same time.
    ///
    /// `keep_alive` specifies the duration for which to keep non-core pool
    /// worker threads alive while they do not receive any work.
    ///
    /// # Panics
    ///
    /// This function will panic if max_size is 0 or lower than core_size.
    pub fn new(core_size: u32, max_size: u32, keep_alive: Duration) -> Self {
        static POOL_COUNTER: AtomicUsize = AtomicUsize::new(1);
        let name = format!("rusty_pool_{}", POOL_COUNTER.fetch_add(1, Ordering::SeqCst));
        ThreadPool::new_named(name, core_size, max_size, keep_alive)
    }

    /// Construct a new `ThreadPool` with the specified name, core pool size, max pool size
    /// and keep_alive time for non-core threads. This function does not spawn any
    /// threads.
    ///
    /// `name` the name of the `ThreadPool` that will be used as prefix for each
    /// thread.
    ///
    /// `core_size` specifies the amount of threads to keep alive for as long as
    /// the `ThreadPool` exists and its channel remains connected.
    ///
    /// `max_size` specifies the maximum number of worker threads that may exist
    /// at the same time.
    ///
    /// `keep_alive` specifies the duration for which to keep non-core pool
    /// worker threads alive while they do not receive any work.
    ///
    /// # Panics
    ///
    /// This function will panic if max_size is 0 or lower than core_size.
    pub fn new_named(name: String, core_size: u32, max_size: u32, keep_alive: Duration) -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();

        if max_size == 0 || max_size < core_size {
            panic!("max_size must be greater than 0 and greater or equal to the core pool size");
        }

        let worker_data = WorkerData {
            pool_name: name,
            worker_count_data: WorkerCountData::default(),
            worker_number: AtomicUsize::new(1),
            join_notify_condvar: Condvar::new(),
            join_notify_mutex: Mutex::new(()),
        };

        let channel_data = ChannelData { sender, receiver };

        Self {
            core_size,
            max_size,
            keep_alive,
            channel_data: Arc::new(channel_data),
            worker_data: Arc::new(worker_data),
        }
    }

    /// Get the number of live workers, includes all workers waiting for work or executing tasks.
    ///
    /// This counter is incremented when creating a new worker even before re-checking whether
    /// the worker is still needed. Once the worker count is updated the previous value returned
    /// by the atomic operation is analysed to check whether it still represents a value that
    /// would require a new worker. If that is not the case this counter will be decremented
    /// and the worker will never spawn a thread and start its working loop. Else this counter
    /// is decremented when a worker reaches the end of its working loop, which for non-core
    /// threads might happen if it does not receive any work during its keep alive time,
    /// for core threads this only happens once the channel is disconnected.
    pub fn get_current_worker_count(&self) -> u32 {
        self.worker_data.worker_count_data.get_total_worker_count()
    }

    /// Get the number of workers currently waiting for work. Those threads are currently
    /// polling from the crossbeam receiver. Core threads wait indefinitely and might remain
    /// in this state until the `ThreadPool` is dropped. The remaining threads give up after
    /// waiting for the specified keep_alive time.
    pub fn get_idle_worker_count(&self) -> u32 {
        self.worker_data.worker_count_data.get_idle_worker_count()
    }

    /// Send a new task to the worker threads. This function is responsible for sending the message through the
    /// channel and creating new workers if needed. If the current worker count is lower than the core pool size
    /// this function will always create a new worker. If the current worker count is equal to or greater than
    /// the core pool size this function only creates a new worker if the worker count is below the max pool size
    /// and there are no idle threads.
    ///
    /// After constructing the worker but before spawning its thread this function checks again whether the new
    /// worker is still needed by analysing the old value returned by atomically incrementing the worker counter
    /// and checking if the worker is still needed or if another thread has already created it,
    /// for non-core threads this additionally checks whether there are idle threads now. When the recheck condition
    /// still applies the new worker will receive the task directly as first task and start executing.
    /// If trying to create a new core worker failed the next step is to try creating a non-core worker instead.
    /// When all checks still fail the task will simply be sent to the main channel instead.
    ///
    /// # Panics
    ///
    /// This function might panic if `try_execute` returns an error when the crossbeam channel has been
    /// closed unexpectedly.
    /// This should never occur under normal circumstances using safe code, as shutting down the `ThreadPool`
    /// consumes ownership and the crossbeam channel is never dropped unless dropping the `ThreadPool`.
    pub fn execute<T: Task<()> + 'static>(&self, task: T) {
        if self.try_execute(task).is_err() {
            panic!("the channel of the thread pool has been closed");
        }
    }

    /// Send a new task to the worker threads. This function is responsible for sending the message through the
    /// channel and creating new workers if needed. If the current worker count is lower than the core pool size
    /// this function will always create a new worker. If the current worker count is equal to or greater than
    /// the core pool size this function only creates a new worker if the worker count is below the max pool size
    /// and there are no idle threads.
    ///
    /// After constructing the worker but before spawning its thread this function checks again whether the new
    /// worker is still needed by analysing the old value returned by atomically incrementing the worker counter
    /// and checking if the worker is still needed or if another thread has already created it,
    /// for non-core threads this additionally checks whether there are idle threads now. When the recheck condition
    /// still applies the new worker will receive the task directly as first task and start executing.
    /// If trying to create a new core worker failed the next step is to try creating a non-core worker instead.
    /// When all checks still fail the task will simply be sent to the main channel instead.
    ///
    /// # Errors
    ///
    /// This function might return `crossbeam_channel::SendError` if the sender was dropped unexpectedly.
    pub fn try_execute<T: Task<()> + 'static>(
        &self,
        task: T,
    ) -> Result<(), crossbeam_channel::SendError<Job>> {
        let is_fn = task.is_fn();
        let task = Box::new(task);
        if is_fn {
            self.try_execute_task(
                task.as_fn()
                    .expect("Task::as_fn returned None despite is_fn returning true"),
            )
        } else {
            self.try_execute_task(Box::new(move || {
                task.run();
            }))
        }
    }

    /// Send a new task to the worker threads and return a [`JoinHandle`](struct.JoinHandle.html) that may be used to await
    /// the result. This function is responsible for sending the message through the channel and creating new
    /// workers if needed. If the current worker count is lower than the core pool size this function will always
    /// create a new worker. If the current worker count is equal to or greater than the core pool size this
    /// function only creates a new worker if the worker count is below the max pool size and there are no idle
    /// threads.
    ///
    /// After constructing the worker but before spawning its thread this function checks again whether the new
    /// worker is still needed by analysing the old value returned by atomically incrementing the worker counter
    /// and checking if the worker is still needed or if another thread has already created it,
    /// for non-core threads this additionally checks whether there are idle threads now. When the recheck condition
    /// still applies the new worker will receive the task directly as first task and start executing.
    /// If trying to create a new core worker failed the next step is to try creating a non-core worker instead.
    /// When all checks still fail the task will simply be sent to the main channel instead.
    ///
    /// # Panics
    ///
    /// This function might panic if `try_execute` returns an error when the crossbeam channel has been
    /// closed unexpectedly.
    /// This should never occur under normal circumstances using safe code, as shutting down the `ThreadPool`
    /// consumes ownership and the crossbeam channel is never dropped unless dropping the `ThreadPool`.
    pub fn evaluate<R: Send + Sync + 'static, T: Task<R> + 'static>(
        &self,
        task: T,
    ) -> JoinHandle<R> {
        match self.try_evaluate(task) {
            Ok(handle) => handle,
            Err(e) => panic!("the channel of the thread pool has been closed: {:?}", e),
        }
    }

    /// Send a new task to the worker threads and return a [`JoinHandle`](struct.JoinHandle.html) that may be used to await
    /// the result. This function is responsible for sending the message through the channel and creating new
    /// workers if needed. If the current worker count is lower than the core pool size this function will always
    /// create a new worker. If the current worker count is equal to or greater than the core pool size this
    /// function only creates a new worker if the worker count is below the max pool size and there are no idle
    /// threads.
    ///
    /// After constructing the worker but before spawning its thread this function checks again whether the new
    /// worker is still needed by analysing the old value returned by atomically incrementing the worker counter
    /// and checking if the worker is still needed or if another thread has already created it,
    /// for non-core threads this additionally checks whether there are idle threads now. When the recheck condition
    /// still applies the new worker will receive the task directly as first task and start executing.
    /// If trying to create a new core worker failed the next step is to try creating a non-core worker instead.
    /// When all checks still fail the task will simply be sent to the main channel instead.
    ///
    /// # Errors
    ///
    /// This function might return `crossbeam_channel::SendError` if the sender was dropped unexpectedly.
    pub fn try_evaluate<R: Send + Sync + 'static, T: Task<R> + 'static>(
        &self,
        task: T,
    ) -> Result<JoinHandle<R>, crossbeam_channel::SendError<Job>> {
        let (sender, receiver) = oneshot::channel::<R>();
        let join_handle = JoinHandle { receiver };
        let task = Box::new(task);
        let job = || {
            let result = task.run();
            // if the receiver was dropped that means the caller was not interested in the result
            let _ignored_result = sender.send(result);
        };

        let execute_attempt = self.try_execute_task(Box::new(job));
        execute_attempt.map(|_| join_handle)
    }

    /// Send a task to the `ThreadPool` that completes the given `Future` and return a [`JoinHandle`](struct.JoinHandle.html)
    /// that may be used to await the result. This function simply calls [`evaluate()`](struct.ThreadPool.html#method.evaluate)
    /// with a closure that calls `block_on` with the provided future.
    ///
    /// # Panic
    ///
    /// This function panics if the task fails to be sent to the `ThreadPool` due to the channel being broken.
    pub fn complete<R: Send + Sync + 'static>(
        &self,
        future: impl Future<Output = R> + 'static + Send + Sync,
    ) -> JoinHandle<R> {
        self.evaluate(|| block_on(future))
    }

    /// Send a task to the `ThreadPool` that completes the given `Future` and return a [`JoinHandle`](struct.JoinHandle.html)
    /// that may be used to await the result. This function simply calls [`try_evaluate()`](struct.ThreadPool.html#method.try_evaluate)
    /// with a closure that calls `block_on` with the provided future.
    ///
    /// # Errors
    ///
    /// This function returns `crossbeam_channel::SendError` if the task fails to be sent to the `ThreadPool` due to the channel being broken.
    pub fn try_complete<R: Send + Sync + 'static>(
        &self,
        future: impl Future<Output = R> + 'static + Send + Sync,
    ) -> Result<JoinHandle<R>, crossbeam_channel::SendError<Job>> {
        self.try_evaluate(|| block_on(future))
    }

    /// Submit a `Future` to be polled by this `ThreadPool`. Unlike [`complete()`](struct.ThreadPool.html#method.complete) this does not
    /// block a worker until the `Future` has been completed but polls the `Future` once at a time and creates a `Waker`
    /// that re-submits the Future to this pool when awakened. Since `Arc<AsyncTask>` implements the [`Task`](trait.Task.html) trait this
    /// function simply constructs the `AsyncTask` and calls [`execute()`](struct.ThreadPool.html#method.execute).
    ///
    /// # Panic
    ///
    /// This function panics if the task fails to be sent to the `ThreadPool` due to the channel being broken.
    #[cfg(feature = "async")]
    pub fn spawn(&self, future: impl Future<Output = ()> + 'static + Send) {
        let future_task = Arc::new(AsyncTask {
            future: Mutex::new(Some(Box::pin(future))),
            pool: self.clone(),
        });

        self.execute(future_task)
    }

    /// Submit a `Future` to be polled by this `ThreadPool`. Unlike [`try_complete()`](struct.ThreadPool.html#method.try_complete) this does not
    /// block a worker until the `Future` has been completed but polls the `Future` once at a time and creates a `Waker`
    /// that re-submits the Future to this pool when awakened. Since `Arc<AsyncTask>` implements the [`Task`](trait.Task.html) trait this
    /// function simply constructs the `AsyncTask` and calls [`try_execute()`](struct.ThreadPool.html#method.try_execute).
    ///
    /// # Errors
    ///
    /// This function returns `crossbeam_channel::SendError` if the task fails to be sent to the `ThreadPool` due to the channel being broken.
    #[cfg(feature = "async")]
    pub fn try_spawn(
        &self,
        future: impl Future<Output = ()> + 'static + Send,
    ) -> Result<(), crossbeam_channel::SendError<Job>> {
        let future_task = Arc::new(AsyncTask {
            future: Mutex::new(Some(Box::pin(future))),
            pool: self.clone(),
        });

        self.try_execute(future_task)
    }

    /// Create a top-level `Future` that awaits the provided `Future` and then sends the result to the
    /// returned [`JoinHandle`](struct.JoinHandle.html). Unlike [`complete()`](struct.ThreadPool.html#method.complete) this does not
    /// block a worker until the `Future` has been completed but polls the `Future` once at a time and creates a `Waker`
    /// that re-submits the Future to this pool when awakened. Since `Arc<AsyncTask>` implements the [`Task`](trait.Task.html) trait this
    /// function simply constructs the `AsyncTask` and calls [`execute()`](struct.ThreadPool.html#method.execute).
    ///
    /// This enables awaiting the final result outside of an async context like [`complete()`](struct.ThreadPool.html#method.complete) while still
    /// polling the future lazily instead of eagerly blocking the worker until the future is done.
    ///
    /// # Panic
    ///
    /// This function panics if the task fails to be sent to the `ThreadPool` due to the channel being broken.
    #[cfg(feature = "async")]
    pub fn spawn_await<R: Send + Sync + 'static>(
        &self,
        future: impl Future<Output = R> + 'static + Send,
    ) -> JoinHandle<R> {
        match self.try_spawn_await(future) {
            Ok(handle) => handle,
            Err(e) => panic!("the channel of the thread pool has been closed: {:?}", e),
        }
    }

    /// Create a top-level `Future` that awaits the provided `Future` and then sends the result to the
    /// returned [`JoinHandle`](struct.JoinHandle.html). Unlike [`try_complete()`](struct.ThreadPool.html#method.try_complete) this does not
    /// block a worker until the `Future` has been completed but polls the `Future` once at a time and creates a `Waker`
    /// that re-submits the Future to this pool when awakened. Since `Arc<AsyncTask>` implements the [`Task`](trait.Task.html) trait this
    /// function simply constructs the `AsyncTask` and calls [`try_execute()`](struct.ThreadPool.html#method.try_execute).
    ///
    /// This enables awaiting the final result outside of an async context like [`complete()`](struct.ThreadPool.html#method.complete) while still
    /// polling the future lazily instead of eagerly blocking the worker until the future is done.
    ///
    /// # Errors
    ///
    /// This function returns `crossbeam_channel::SendError` if the task fails to be sent to the `ThreadPool` due to the channel being broken.
    #[cfg(feature = "async")]
    pub fn try_spawn_await<R: Send + Sync + 'static>(
        &self,
        future: impl Future<Output = R> + 'static + Send,
    ) -> Result<JoinHandle<R>, crossbeam_channel::SendError<Job>> {
        let (sender, receiver) = oneshot::channel::<R>();
        let join_handle = JoinHandle { receiver };

        self.try_spawn(async {
            let result = future.await;
            // if the receiver was dropped that means the caller was not interested in the result
            let _ignored_result = sender.send(result);
        })
        .map(|_| join_handle)
    }

    #[inline]
    fn try_execute_task(&self, task: Job) -> Result<(), crossbeam_channel::SendError<Job>> {
        // create a new worker either if the current worker count is lower than the core pool size
        // or if there are no idle threads and the current worker count is lower than the max pool size
        let (curr_worker_count, idle_worker_count) = self.worker_data.worker_count_data.get_both();

        if curr_worker_count < self.core_size {
            if let Err(task) =
                self.create_worker(true, task, |_, old_val| old_val.0 < self.core_size)
            {
                return self.send_task_to_channel(task);
            }
        } else if curr_worker_count < self.max_size && idle_worker_count == 0 {
            if let Err(task) = self.create_worker(false, task, ThreadPool::recheck_non_core) {
                return self.send_task_to_channel(task);
            }
        } else {
            return self.send_task_to_channel(task);
        }

        Ok(())
    }

    /// Blocks the current thread until there aren't any non-idle threads anymore.
    /// This includes work started after calling this function.
    /// This function blocks until the next time this `ThreadPool` completes all of its work,
    /// except if all threads are idle and the channel is empty at the time of calling this
    /// function, in which case it will fast-return.
    ///
    /// This utilizes a `Condvar` that is notified by workers when they complete a job and notice
    /// that the channel is currently empty and it was the last thread to finish the current
    /// generation of work (i.e. when incrementing the idle worker counter brings the value
    /// up to the total worker counter, meaning it's the last thread to become idle).
    pub fn join(&self) {
        self.inner_join(None);
    }

    /// Blocks the current thread until there aren't any non-idle threads anymore or until the
    /// specified time_out Duration passes, whichever happens first.
    /// This includes work started after calling this function.
    /// This function blocks until the next time this `ThreadPool` completes all of its work,
    /// (or until the time_out is reached) except if all threads are idle and the channel is
    /// empty at the time of calling this function, in which case it will fast-return.
    ///
    /// This utilizes a `Condvar` that is notified by workers when they complete a job and notice
    /// that the channel is currently empty and it was the last thread to finish the current
    /// generation of work (i.e. when incrementing the idle worker counter brings the value
    /// up to the total worker counter, meaning it's the last thread to become idle).
    pub fn join_timeout(&self, time_out: Duration) {
        self.inner_join(Some(time_out));
    }

    /// Destroy this `ThreadPool` by claiming ownership and dropping the value,
    /// causing the `Sender` to drop thus disconnecting the channel.
    /// Threads in this pool that are currently executing a task will finish what
    /// they're doing until they check the channel, discovering that it has been
    /// disconnected from the sender and thus terminate their work loop.
    ///
    /// If other clones of this `ThreadPool` exist the sender will remain intact
    /// and tasks submitted to those clones will succeed, this includes pending
    /// `AsyncTask` instances as they hold an owned clone of the `ThreadPool`
    /// to re-submit awakened futures.
    pub fn shutdown(self) {
        drop(self);
    }

    /// Destroy this `ThreadPool` by claiming ownership and dropping the value,
    /// causing the `Sender` to drop thus disconnecting the channel.
    /// Threads in this pool that are currently executing a task will finish what
    /// they're doing until they check the channel, discovering that it has been
    /// disconnected from the sender and thus terminate their work loop.
    ///
    /// If other clones of this `ThreadPool` exist the sender will remain intact
    /// and tasks submitted to those clones will succeed, this includes pending
    /// `AsyncTask` instances as they hold an owned clone of the `ThreadPool`
    /// to re-submit awakened futures.
    ///
    /// This function additionally joins all workers after dropping the pool to
    /// wait for all work to finish.
    /// Blocks the current thread until there aren't any non-idle threads anymore.
    /// This function blocks until this `ThreadPool` completes all of its work,
    /// except if all threads are idle and the channel is empty at the time of
    /// calling this function, in which case the join will fast-return.
    /// If other live clones of this `ThreadPool` exist this behaves the same as
    /// calling [`join`](struct.ThreadPool.html#method.join) on a live `ThreadPool` as tasks submitted
    /// to one of the clones will be joined as well.
    ///
    /// The join utilizes a `Condvar` that is notified by workers when they complete a job and notice
    /// that the channel is currently empty and it was the last thread to finish the current
    /// generation of work (i.e. when incrementing the idle worker counter brings the value
    /// up to the total worker counter, meaning it's the last thread to become idle).
    pub fn shutdown_join(self) {
        self.inner_shutdown_join(None);
    }

    /// Destroy this `ThreadPool` by claiming ownership and dropping the value,
    /// causing the `Sender` to drop thus disconnecting the channel.
    /// Threads in this pool that are currently executing a task will finish what
    /// they're doing until they check the channel, discovering that it has been
    /// disconnected from the sender and thus terminate their work loop.
    ///
    /// If other clones of this `ThreadPool` exist the sender will remain intact
    /// and tasks submitted to those clones will succeed, this includes pending
    /// `AsyncTask` instances as they hold an owned clone of the `ThreadPool`
    /// to re-submit awakened futures.
    ///
    /// This function additionally joins all workers after dropping the pool to
    /// wait for all work to finish.
    /// Blocks the current thread until there aren't any non-idle threads anymore or until the
    /// specified time_out Duration passes, whichever happens first.
    /// This function blocks until this `ThreadPool` completes all of its work,
    /// (or until the time_out is reached) except if all threads are idle and the channel is
    /// empty at the time of calling this function, in which case the join will fast-return.
    /// If other live clones of this `ThreadPool` exist this behaves the same as
    /// calling [`join`](struct.ThreadPool.html#method.join) on a live `ThreadPool` as tasks submitted
    /// to one of the clones will be joined as well.
    ///
    /// The join utilizes a `Condvar` that is notified by workers when they complete a job and notice
    /// that the channel is currently empty and it was the last thread to finish the current
    /// generation of work (i.e. when incrementing the idle worker counter brings the value
    /// up to the total worker counter, meaning it's the last thread to become idle).
    pub fn shutdown_join_timeout(self, timeout: Duration) {
        self.inner_shutdown_join(Some(timeout));
    }

    /// Return the name of this pool, used as prefix for each worker thread.
    pub fn get_name(&self) -> &str {
        &self.worker_data.pool_name
    }

    /// Create a new worker then check the `recheck_condition`. If this still applies give the worker
    /// the submitted task directly as its initial task. Else this method returns the task, which will
    /// be given to the main channel instead.
    fn create_worker<C>(&self, is_core: bool, task: Job, recheck_condition: C) -> Result<(), Job>
    where
        C: Fn(&ThreadPool, (u32, u32)) -> bool,
    {
        let worker = Worker::new(
            self.channel_data.receiver.clone(),
            Arc::clone(&self.worker_data),
            !is_core,
            if is_core { None } else { Some(self.keep_alive) },
        );

        let old_val = self
            .worker_data
            .worker_count_data
            .increment_worker_total_ret_both();
        if recheck_condition(&self, old_val) {
            // new worker is still needed after checking again, give it the task and spawn the thread
            worker.start(Some(task));
        } else {
            // recheck condition does not apply anymore, either there is an idle thread now (and is_core is false)
            // or the worker has already been created by another thread
            self.worker_data.worker_count_data.decrement_worker_total();

            if is_core && old_val.0 < self.max_size && old_val.1 == 0 {
                // if trying to create core thread failed try creating non-core thread instead

                // have to use function pointer instead of closure due to recursive call
                return self.create_worker(false, task, ThreadPool::recheck_non_core);
            }
            return Err(task);
        }

        Ok(())
    }

    #[inline]
    fn send_task_to_channel(&self, task: Job) -> Result<(), crossbeam_channel::SendError<Job>> {
        self.channel_data.sender.send(task)?;

        Ok(())
    }

    #[inline]
    fn inner_join(&self, time_out: Option<Duration>) {
        ThreadPool::_do_join(&self.worker_data, &self.channel_data.receiver, time_out);
    }

    #[inline]
    fn inner_shutdown_join(self, timeout: Option<Duration>) {
        let current_worker_data = self.worker_data.clone();
        let receiver = self.channel_data.receiver.clone();
        drop(self);
        ThreadPool::_do_join(&current_worker_data, &receiver, timeout);
    }

    #[inline]
    fn _do_join(
        current_worker_data: &Arc<WorkerData>,
        receiver: &crossbeam_channel::Receiver<Job>,
        time_out: Option<Duration>,
    ) {
        let (current_worker_count, current_idle_count) =
            current_worker_data.worker_count_data.get_both();
        // no thread is currently doing any work, return
        if current_idle_count == current_worker_count && receiver.is_empty() {
            return;
        }

        let guard = current_worker_data
            .join_notify_mutex
            .lock()
            .expect("could not get join notify mutex lock");

        match time_out {
            Some(time_out) => {
                let _ret_lock = current_worker_data
                    .join_notify_condvar
                    .wait_timeout(guard, time_out)
                    .expect("could not wait for join condvar");
            }
            None => {
                let _ret_lock = current_worker_data
                    .join_notify_condvar
                    .wait(guard)
                    .expect("could not wait for join condvar");
            }
        };
    }

    fn recheck_non_core(&self, old_val: (u32, u32)) -> bool {
        let (old_worker_total, old_worker_idle) = old_val;
        old_worker_total < self.max_size && old_worker_idle == 0
    }
}

impl Default for ThreadPool {
    /// create default ThreadPool with the core pool size being equal to the number of cpus
    /// and the max_size being twice the core size with a 60 second timeout
    fn default() -> Self {
        let num_cpus = num_cpus::get() as u32;
        ThreadPool::new(num_cpus, num_cpus * 4, Duration::from_secs(60))
    }
}

/// A helper struct to aid creating a new `ThreadPool` using default values where no value was
/// explicitly specified.
#[derive(Default)]
pub struct Builder {
    name: Option<String>,
    core_size: Option<u32>,
    max_size: Option<u32>,
    keep_alive: Option<Duration>,
}

impl Builder {
    /// Create a new `Builder`.
    pub fn new() -> Builder {
        Builder::default()
    }

    /// Specify the name of the `ThreadPool` that will be used as prefix for the name of each worker thread.
    /// By default the name is "rusty_pool_x" with x being a static pool counter.
    pub fn name(mut self, name: String) -> Builder {
        self.name = Some(name);
        self
    }

    /// Specify the core pool size for the `ThreadPool`. The core pool size is the number of threads that stay alive
    /// for the entire lifetime of the `ThreadPool` or, to be more precise, its channel. These threads are spawned if
    /// a task is submitted to the `ThreadPool` and the current worker count is below the core pool size.
    pub fn core_size(mut self, size: u32) -> Builder {
        self.core_size = Some(size);
        self
    }

    /// Specify the maximum pool size this `ThreadPool` may scale up to. This numbers represents the maximum number
    /// of threads that may be alive at the same time within this pool. Additional threads above the core pool size
    /// only remain idle for the duration specified by the `keep_alive` parameter before terminating. If the core pool
    /// is full, the current pool size is below the max size and there are no idle threads then additional threads
    /// will be spawned.
    pub fn max_size(mut self, size: u32) -> Builder {
        self.max_size = Some(size);
        self
    }

    /// Specify the duration for which additional threads outside the core pool remain alive while not receiving any
    /// work before giving up and terminating.
    pub fn keep_alive(mut self, keep_alive: Duration) -> Builder {
        self.keep_alive = Some(keep_alive);
        self
    }

    /// Build the `ThreadPool` using the parameters previously supplied to this `Builder` using the number of CPUs as
    /// default core size if none provided, 4 times the core size as max size if none provided, 60 seconds keep_alive
    /// if none provided and the default naming if none provided. This function calls [`ThreadPool::new`](struct.ThreadPool.html#method.new)
    /// or [`ThreadPool::new_named`](struct.ThreadPool.html#method.new_named) depending on whether a name was provided.
    ///
    /// # Panics
    ///
    /// Building might panic if the `max_size` is 0 or lower than `core_size`:
    pub fn build(self) -> ThreadPool {
        let core_size = self.core_size.unwrap_or(num_cpus::get() as u32);
        let max_size = self.max_size.unwrap_or(core_size * 4);
        let keep_alive = self.keep_alive.unwrap_or(Duration::from_secs(60));

        if let Some(name) = self.name {
            ThreadPool::new_named(name, core_size, max_size, keep_alive)
        } else {
            ThreadPool::new(core_size, max_size, keep_alive)
        }
    }
}

#[derive(Clone)]
struct Worker {
    receiver: crossbeam_channel::Receiver<Job>,
    worker_data: Arc<WorkerData>,
    can_timeout: bool,
    keep_alive: Option<Duration>,
}

impl Worker {
    fn new(
        receiver: crossbeam_channel::Receiver<Job>,
        worker_data: Arc<WorkerData>,
        can_timeout: bool,
        keep_alive: Option<Duration>,
    ) -> Self {
        Worker {
            receiver,
            worker_data,
            can_timeout,
            keep_alive,
        }
    }

    fn start(self, task: Option<Job>) {
        let worker_name = format!(
            "{}_thread_{}",
            self.worker_data.pool_name,
            self.worker_data
                .worker_number
                .fetch_add(1, Ordering::SeqCst)
        );

        thread::Builder::new()
            .name(worker_name)
            .spawn(move || {
                let mut sentinel = Sentinel::new(&self);

                if let Some(task) = task {
                    self.exec_task_and_notify(&mut sentinel, task);
                }

                loop {
                    // the two functions return different error types, but since the error type doesn't matter it is mapped to unit to make them compatible
                    let received_task: Result<Job, _> = if self.can_timeout {
                        self.receiver
                            .recv_timeout(self.keep_alive.expect(
                                "keep_alive duration is NONE despite can_timeout being true",
                            ))
                            .map_err(|_| ())
                    } else {
                        self.receiver.recv().map_err(|_| ())
                    };

                    match received_task {
                        Ok(task) => {
                            // mark current as no longer idle and execute task
                            self.worker_data.worker_count_data.decrement_worker_idle();
                            self.exec_task_and_notify(&mut sentinel, task);
                        }
                        Err(_) => {
                            // either channel was broken because the sender disconnected or, if can_timeout is true, the Worker has not received any work during
                            // its keep_alive period and will now terminate, break working loop
                            break;
                        }
                    }
                }

                // can decrement both at once as the thread only gets here from an idle state
                // (if waiting for work and receiving an error)
                self.worker_data.worker_count_data.decrement_both();
            })
            .expect("could not spawn thread");
    }

    #[inline]
    fn exec_task_and_notify(&self, sentinel: &mut Sentinel, task: Job) {
        sentinel.is_working = true;
        task();
        sentinel.is_working = false;
        // can already mark as idle as this thread will continue the work loop
        self.mark_idle_and_notify_joiners_if_no_work();
    }

    #[inline]
    fn mark_idle_and_notify_joiners_if_no_work(&self) {
        let (old_total_count, old_idle_count) = self
            .worker_data
            .worker_count_data
            .increment_worker_idle_ret_both();
        if self.receiver.is_empty() {
            // if the last task was the last one in the current generation,
            // i.e. if incrementing the idle count leads to the idle count
            // being equal to the total worker count, notify joiners
            if old_total_count == old_idle_count + 1 {
                let _lock = self
                    .worker_data
                    .join_notify_mutex
                    .lock()
                    .expect("could not get join notify mutex lock");
                self.worker_data.join_notify_condvar.notify_all();
            }
        }
    }
}

/// Type that exists to manage worker exit on panic.
///
/// This type is constructed once per `Worker` and implements `Drop` to handle proper worker exit
/// in case the worker panics when executing the current task or anywhere else in its work loop.
/// If the `Sentinel` is dropped at the end of the worker's work loop and the current thread is
/// panicking, handle worker exit the same way as if the task completed normally (if the worker
/// panicked while executing a submitted task) then clone the worker and start it with an initial
/// task of `None`.
struct Sentinel<'s> {
    is_working: bool,
    worker_ref: &'s Worker,
}

impl Sentinel<'_> {
    fn new(worker_ref: &Worker) -> Sentinel<'_> {
        Sentinel {
            is_working: false,
            worker_ref,
        }
    }
}

impl Drop for Sentinel<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            if self.is_working {
                // worker thread panicked in the process of executing a submitted task,
                // run the same logic as if the task completed normally and mark it as
                // idle, since a clone of this task will start the work loop as idle
                // thread
                self.worker_ref.mark_idle_and_notify_joiners_if_no_work();
            }

            let worker = self.worker_ref.clone();
            worker.start(None);
        }
    }
}

const WORKER_IDLE_MASK: u64 = 0x0000_0000_FFFF_FFFF;

/// Struct that stores and handles an `AtomicU64` that stores the total worker count
/// in the higher 32 bits and the idle worker count in the lower 32 bits.
/// This allows to to increment / decrement both counters in a single atomic operation.
#[derive(Default)]
struct WorkerCountData {
    worker_count: AtomicU64,
}

impl WorkerCountData {
    fn get_total_worker_count(&self) -> u32 {
        let curr_val = self.worker_count.load(Ordering::SeqCst);
        WorkerCountData::get_total_count(curr_val)
    }

    fn get_idle_worker_count(&self) -> u32 {
        let curr_val = self.worker_count.load(Ordering::SeqCst);
        WorkerCountData::get_idle_count(curr_val)
    }

    fn get_both(&self) -> (u32, u32) {
        let curr_val = self.worker_count.load(Ordering::SeqCst);
        WorkerCountData::split(curr_val)
    }

    // keep for testing and completion's sake
    #[allow(dead_code)]
    fn increment_both(&self) -> (u32, u32) {
        let old_val = self
            .worker_count
            .fetch_add(0x0000_0001_0000_0001, Ordering::SeqCst);
        WorkerCountData::split(old_val)
    }

    fn decrement_both(&self) -> (u32, u32) {
        let old_val = self
            .worker_count
            .fetch_sub(0x0000_0001_0000_0001, Ordering::SeqCst);
        WorkerCountData::split(old_val)
    }

    // keep for testing and completion's sake
    #[allow(dead_code)]
    fn increment_worker_total(&self) -> u32 {
        let old_val = self
            .worker_count
            .fetch_add(0x0000_0001_0000_0000, Ordering::SeqCst);
        WorkerCountData::get_total_count(old_val)
    }

    // function that only increments the total worker count but return the old
    // values of both fields. Used for the recheck when creating a new worker.
    fn increment_worker_total_ret_both(&self) -> (u32, u32) {
        let old_val = self
            .worker_count
            .fetch_add(0x0000_0001_0000_0000, Ordering::SeqCst);
        WorkerCountData::split(old_val)
    }

    fn decrement_worker_total(&self) -> u32 {
        let old_val = self
            .worker_count
            .fetch_sub(0x0000_0001_0000_0000, Ordering::SeqCst);
        WorkerCountData::get_total_count(old_val)
    }

    // keep for testing and completion's sake
    #[allow(dead_code)]
    fn decrement_worker_total_ret_both(&self) -> (u32, u32) {
        let old_val = self
            .worker_count
            .fetch_sub(0x0000_0001_0000_0000, Ordering::SeqCst);
        WorkerCountData::split(old_val)
    }

    // keep for testing and completion's sake
    #[allow(dead_code)]
    fn increment_worker_idle(&self) -> u32 {
        let old_val = self
            .worker_count
            .fetch_add(0x0000_0000_0000_0001, Ordering::SeqCst);
        WorkerCountData::get_idle_count(old_val)
    }

    fn increment_worker_idle_ret_both(&self) -> (u32, u32) {
        let old_val = self
            .worker_count
            .fetch_add(0x0000_0000_0000_0001, Ordering::SeqCst);
        WorkerCountData::split(old_val)
    }

    fn decrement_worker_idle(&self) -> u32 {
        let old_val = self
            .worker_count
            .fetch_sub(0x0000_0000_0000_0001, Ordering::SeqCst);
        WorkerCountData::get_idle_count(old_val)
    }

    // keep for testing and completion's sake
    #[allow(dead_code)]
    fn decrement_worker_idle_ret_both(&self) -> (u32, u32) {
        let old_val = self
            .worker_count
            .fetch_sub(0x0000_0000_0000_0001, Ordering::SeqCst);
        WorkerCountData::split(old_val)
    }

    #[inline]
    fn split(val: u64) -> (u32, u32) {
        let total_count = (val >> 32) as u32;
        let idle_count = (val & WORKER_IDLE_MASK) as u32;
        (total_count, idle_count)
    }

    #[inline]
    fn get_total_count(val: u64) -> u32 {
        (val >> 32) as u32
    }

    #[inline]
    fn get_idle_count(val: u64) -> u32 {
        // upper 32 bits are ommitted anyway
        (val & WORKER_IDLE_MASK) as u32
    }
}

/// struct containing data shared between workers
struct WorkerData {
    pool_name: String,
    worker_count_data: WorkerCountData,
    worker_number: AtomicUsize,
    join_notify_condvar: Condvar,
    join_notify_mutex: Mutex<()>,
}

struct ChannelData {
    sender: crossbeam_channel::Sender<Job>,
    receiver: crossbeam_channel::Receiver<Job>,
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::thread;
    use std::time::Duration;

    use super::Builder;
    use super::ThreadPool;
    use super::WorkerCountData;

    #[test]
    fn it_works() {
        let pool = ThreadPool::new(2, 10, Duration::from_secs(5));
        let count = Arc::new(AtomicUsize::new(0));

        let count1 = count.clone();
        pool.execute(move || {
            count1.fetch_add(1, Ordering::SeqCst);
            thread::sleep(std::time::Duration::from_secs(4));
        });
        let count2 = count.clone();
        pool.execute(move || {
            count2.fetch_add(1, Ordering::SeqCst);
            thread::sleep(std::time::Duration::from_secs(4));
        });
        let count3 = count.clone();
        pool.execute(move || {
            count3.fetch_add(1, Ordering::SeqCst);
            thread::sleep(std::time::Duration::from_secs(4));
        });
        let count4 = count.clone();
        pool.execute(move || {
            count4.fetch_add(1, Ordering::SeqCst);
            thread::sleep(std::time::Duration::from_secs(4));
        });
        thread::sleep(std::time::Duration::from_secs(20));
        let count5 = count.clone();
        pool.execute(move || {
            count5.fetch_add(1, Ordering::SeqCst);
            thread::sleep(std::time::Duration::from_secs(4));
        });
        let count6 = count.clone();
        pool.execute(move || {
            count6.fetch_add(1, Ordering::SeqCst);
            thread::sleep(std::time::Duration::from_secs(4));
        });
        let count7 = count.clone();
        pool.execute(move || {
            count7.fetch_add(1, Ordering::SeqCst);
            thread::sleep(std::time::Duration::from_secs(4));
        });
        let count8 = count.clone();
        pool.execute(move || {
            count8.fetch_add(1, Ordering::SeqCst);
            thread::sleep(std::time::Duration::from_secs(4));
        });
        thread::sleep(std::time::Duration::from_secs(20));

        let count = count.load(Ordering::SeqCst);
        let worker_count = pool.get_current_worker_count();

        assert_eq!(count, 8);
        // assert that non-core threads were dropped
        assert_eq!(worker_count, 2);
        assert_eq!(pool.get_idle_worker_count(), 2);
    }

    #[test]
    #[ignore]
    fn stress_test() {
        let pool = Arc::new(ThreadPool::new(3, 50, Duration::from_secs(30)));
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..5 {
            let pool_1 = pool.clone();
            let clone = counter.clone();
            pool.execute(move || {
                for _ in 0..160 {
                    let clone = clone.clone();
                    pool_1.execute(move || {
                        clone.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_secs(10));
                    });
                }

                thread::sleep(Duration::from_secs(20));

                for _ in 0..160 {
                    let clone = clone.clone();
                    pool_1.execute(move || {
                        clone.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_secs(10));
                    });
                }
            });
        }

        thread::sleep(Duration::from_secs(10));
        assert_eq!(pool.get_current_worker_count(), 50);

        pool.join();
        assert_eq!(counter.load(Ordering::SeqCst), 1600);
        thread::sleep(Duration::from_secs(31));
        assert_eq!(pool.get_current_worker_count(), 3);
    }

    #[test]
    fn test_join() {
        // use a thread pool with one thread max to make sure the second task starts after
        // pool.join() is called to make sure it joins future tasks as well
        let pool = ThreadPool::new(0, 1, Duration::from_secs(5));
        let counter = Arc::new(AtomicUsize::new(0));

        let clone_1 = counter.clone();
        pool.execute(move || {
            thread::sleep(Duration::from_secs(5));
            clone_1.fetch_add(1, Ordering::SeqCst);
        });

        let clone_2 = counter.clone();
        pool.execute(move || {
            thread::sleep(Duration::from_secs(5));
            clone_2.fetch_add(1, Ordering::SeqCst);
        });

        pool.join();

        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_join_timeout() {
        let pool = ThreadPool::new(0, 1, Duration::from_secs(5));
        let counter = Arc::new(AtomicUsize::new(0));

        let clone = counter.clone();
        pool.execute(move || {
            thread::sleep(Duration::from_secs(10));
            clone.fetch_add(1, Ordering::SeqCst);
        });

        pool.join_timeout(Duration::from_secs(5));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_shutdown() {
        let pool = ThreadPool::new(1, 3, Duration::from_secs(5));
        let counter = Arc::new(AtomicUsize::new(0));

        let clone_1 = counter.clone();
        pool.execute(move || {
            thread::sleep(Duration::from_secs(5));
            clone_1.fetch_add(1, Ordering::SeqCst);
        });

        let clone_2 = counter.clone();
        pool.execute(move || {
            thread::sleep(Duration::from_secs(5));
            clone_2.fetch_add(1, Ordering::SeqCst);
        });

        let clone_3 = counter.clone();
        pool.execute(move || {
            thread::sleep(Duration::from_secs(5));
            clone_3.fetch_add(1, Ordering::SeqCst);
        });

        // since the pool only allows three threads this won't get the chance to run
        let clone_4 = counter.clone();
        pool.execute(move || {
            thread::sleep(Duration::from_secs(5));
            clone_4.fetch_add(1, Ordering::SeqCst);
        });

        pool.join_timeout(Duration::from_secs(2));
        pool.shutdown();

        thread::sleep(Duration::from_secs(5));

        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[should_panic(
        expected = "max_size must be greater than 0 and greater or equal to the core pool size"
    )]
    #[test]
    fn test_panic_on_0_max_pool_size() {
        ThreadPool::new(0, 0, Duration::from_secs(2));
    }

    #[should_panic(
        expected = "max_size must be greater than 0 and greater or equal to the core pool size"
    )]
    #[test]
    fn test_panic_on_smaller_max_than_core_pool_size() {
        ThreadPool::new(0, 0, Duration::from_secs(2));
    }

    #[test]
    fn test_empty_join() {
        let pool = ThreadPool::new(3, 10, Duration::from_secs(10));
        pool.join();
    }

    #[test]
    fn test_join_when_complete() {
        let pool = ThreadPool::new(3, 10, Duration::from_secs(5));

        pool.execute(|| {
            thread::sleep(Duration::from_millis(5000));
        });

        thread::sleep(Duration::from_millis(5000));
        pool.join();
    }

    #[test]
    fn test_full_usage() {
        let pool = ThreadPool::new(5, 50, Duration::from_secs(10));

        for _ in 0..100 {
            pool.execute(|| {
                thread::sleep(Duration::from_secs(30));
            });
        }

        thread::sleep(Duration::from_secs(10));
        assert_eq!(pool.get_current_worker_count(), 50);

        pool.join();
        thread::sleep(Duration::from_secs(15));
        assert_eq!(pool.get_current_worker_count(), 5);
    }

    #[test]
    fn test_shutdown_join() {
        let pool = ThreadPool::new(1, 1, Duration::from_secs(5));
        let counter = Arc::new(AtomicUsize::new(0));

        let clone = counter.clone();
        pool.execute(move || {
            thread::sleep(Duration::from_secs(10));
            clone.fetch_add(1, Ordering::SeqCst);
        });

        pool.shutdown_join();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_shutdown_join_timeout() {
        let pool = ThreadPool::new(1, 1, Duration::from_secs(5));
        let counter = Arc::new(AtomicUsize::new(0));

        let clone = counter.clone();
        pool.execute(move || {
            thread::sleep(Duration::from_secs(10));
            clone.fetch_add(1, Ordering::SeqCst);
        });

        pool.shutdown_join_timeout(Duration::from_secs(5));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_empty_shutdown_join() {
        let pool = ThreadPool::new(1, 5, Duration::from_secs(5));
        pool.shutdown_join();
    }

    #[test]
    fn test_shutdown_on_complete() {
        let pool = ThreadPool::new(3, 10, Duration::from_secs(5));

        pool.execute(|| {
            thread::sleep(Duration::from_millis(5000));
        });

        thread::sleep(Duration::from_millis(5000));
        pool.shutdown_join();
    }

    #[test]
    fn test_shutdown_after_complete() {
        let pool = ThreadPool::new(3, 10, Duration::from_secs(5));

        pool.execute(|| {
            thread::sleep(Duration::from_millis(5000));
        });

        thread::sleep(Duration::from_millis(7000));
        pool.shutdown_join();
    }

    #[test]
    fn worker_count_test() {
        let worker_count_data = WorkerCountData::default();

        assert_eq!(worker_count_data.get_total_worker_count(), 0);
        assert_eq!(worker_count_data.get_idle_worker_count(), 0);

        worker_count_data.increment_both();

        assert_eq!(worker_count_data.get_total_worker_count(), 1);
        assert_eq!(worker_count_data.get_idle_worker_count(), 1);

        for _ in 0..10 {
            worker_count_data.increment_both();
        }

        assert_eq!(worker_count_data.get_total_worker_count(), 11);
        assert_eq!(worker_count_data.get_idle_worker_count(), 11);

        for _ in 0..15 {
            worker_count_data.increment_worker_total();
        }

        for _ in 0..7 {
            worker_count_data.increment_worker_idle();
        }

        assert_eq!(worker_count_data.get_total_worker_count(), 26);
        assert_eq!(worker_count_data.get_idle_worker_count(), 18);
        assert_eq!(worker_count_data.get_both(), (26, 18));

        for _ in 0..5 {
            worker_count_data.decrement_both();
        }

        assert_eq!(worker_count_data.get_total_worker_count(), 21);
        assert_eq!(worker_count_data.get_idle_worker_count(), 13);

        for _ in 0..13 {
            worker_count_data.decrement_worker_total();
        }

        for _ in 0..4 {
            worker_count_data.decrement_worker_idle();
        }

        assert_eq!(worker_count_data.get_total_worker_count(), 8);
        assert_eq!(worker_count_data.get_idle_worker_count(), 9);

        for _ in 0..456789 {
            worker_count_data.increment_worker_total();
        }

        assert_eq!(worker_count_data.get_total_worker_count(), 456797);
        assert_eq!(worker_count_data.get_idle_worker_count(), 9);
        assert_eq!(worker_count_data.get_both(), (456797, 9));

        for _ in 0..23456 {
            worker_count_data.increment_worker_idle();
        }

        assert_eq!(worker_count_data.get_total_worker_count(), 456797);
        assert_eq!(worker_count_data.get_idle_worker_count(), 23465);

        for _ in 0..150000 {
            worker_count_data.decrement_worker_total();
        }

        assert_eq!(worker_count_data.get_total_worker_count(), 306797);
        assert_eq!(worker_count_data.get_idle_worker_count(), 23465);

        for _ in 0..10000 {
            worker_count_data.decrement_worker_idle();
        }

        assert_eq!(worker_count_data.get_total_worker_count(), 306797);
        assert_eq!(worker_count_data.get_idle_worker_count(), 13465);
    }

    #[test]
    fn test_join_enqueued_task() {
        let pool = ThreadPool::new(3, 50, Duration::from_secs(20));
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..160 {
            let clone = counter.clone();
            pool.execute(move || {
                thread::sleep(Duration::from_secs(10));
                clone.fetch_add(1, Ordering::SeqCst);
            });
        }

        thread::sleep(Duration::from_secs(5));
        assert_eq!(pool.get_current_worker_count(), 50);

        pool.join();
        assert_eq!(counter.load(Ordering::SeqCst), 160);
        thread::sleep(Duration::from_secs(21));
        assert_eq!(pool.get_current_worker_count(), 3);
    }

    #[test]
    fn test_panic_all() {
        let pool = ThreadPool::new(3, 10, Duration::from_secs(2));

        for _ in 0..10 {
            pool.execute(|| {
                panic!("test");
            })
        }

        pool.join();
        thread::sleep(Duration::from_secs(5));
        assert_eq!(pool.get_current_worker_count(), 3);
        assert_eq!(pool.get_idle_worker_count(), 3);
    }

    #[test]
    fn test_panic_some() {
        let pool = ThreadPool::new(3, 10, Duration::from_secs(5));
        let counter = Arc::new(AtomicUsize::new(0));

        for i in 0..10 {
            let clone = counter.clone();
            pool.execute(move || {
                if i < 3 || i % 2 == 0 {
                    thread::sleep(Duration::from_secs(5));
                    clone.fetch_add(1, Ordering::SeqCst);
                } else {
                    thread::sleep(Duration::from_secs(5));
                    panic!("test");
                }
            })
        }

        pool.join();
        assert_eq!(counter.load(Ordering::SeqCst), 6);
        assert_eq!(pool.get_current_worker_count(), 10);
        assert_eq!(pool.get_idle_worker_count(), 10);
        thread::sleep(Duration::from_secs(10));
        assert_eq!(pool.get_current_worker_count(), 3);
        assert_eq!(pool.get_idle_worker_count(), 3);
    }

    #[test]
    fn test_panic_all_core_threads() {
        let pool = ThreadPool::new(3, 3, Duration::from_secs(1));
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            pool.execute(|| {
                panic!("test");
            })
        }

        pool.join();

        for i in 0..10 {
            let clone = counter.clone();
            pool.execute(move || {
                if i < 3 || i % 2 == 0 {
                    clone.fetch_add(1, Ordering::SeqCst);
                } else {
                    thread::sleep(Duration::from_secs(5));
                    panic!("test");
                }
            })
        }

        pool.join();
        assert_eq!(counter.load(Ordering::SeqCst), 6);
        assert_eq!(pool.get_current_worker_count(), 3);
        assert_eq!(pool.get_idle_worker_count(), 3);
    }

    #[test]
    fn test_drop_all_receivers() {
        let pool = ThreadPool::new(0, 3, Duration::from_secs(5));
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let clone = counter.clone();
            pool.execute(move || {
                clone.fetch_add(1, Ordering::SeqCst);
            })
        }

        pool.join();
        assert_eq!(counter.load(Ordering::SeqCst), 3);
        thread::sleep(Duration::from_secs(10));
        assert_eq!(pool.get_current_worker_count(), 0);

        for _ in 0..3 {
            let clone = counter.clone();
            pool.execute(move || {
                clone.fetch_add(1, Ordering::SeqCst);
            })
        }

        pool.join();
        assert_eq!(counter.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn test_evaluate() {
        let pool = ThreadPool::new(0, 3, Duration::from_secs(5));

        let count = AtomicUsize::new(0);

        let handle = pool.evaluate(move || {
            count.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_secs(5));
            count.fetch_add(1, Ordering::SeqCst)
        });

        let result = handle.await_complete();
        assert_eq!(result, 1);
    }

    #[test]
    fn test_multiple_evaluate() {
        let pool = ThreadPool::new(0, 3, Duration::from_secs(5));

        let count = AtomicUsize::new(0);
        let handle_1 = pool.evaluate(move || {
            for _ in 0..10000 {
                count.fetch_add(1, Ordering::SeqCst);
            }

            thread::sleep(Duration::from_secs(5));

            for _ in 0..10000 {
                count.fetch_add(1, Ordering::SeqCst);
            }

            count.load(Ordering::SeqCst)
        });

        let handle_2 = pool.evaluate(move || {
            let result = handle_1.await_complete();
            let count = AtomicUsize::new(result);

            for _ in 0..15000 {
                count.fetch_add(1, Ordering::SeqCst);
            }

            thread::sleep(Duration::from_secs(5));

            for _ in 0..20000 {
                count.fetch_add(1, Ordering::SeqCst);
            }

            count.load(Ordering::SeqCst)
        });

        let result = handle_2.await_complete();
        assert_eq!(result, 55000);
    }

    #[should_panic(expected = "could not receive message because channel was cancelled")]
    #[test]
    fn test_evaluate_panic() {
        let pool = Builder::new().core_size(5).max_size(50).build();

        let handle = pool.evaluate(|| {
            let x = 3;

            if x == 3 {
                panic!("expected panic")
            }

            return x;
        });

        handle.await_complete();
    }

    #[test]
    fn test_complete_fut() {
        let pool = ThreadPool::new(0, 3, Duration::from_secs(5));

        async fn async_fn() -> i8 {
            8
        }

        let fut = async_fn();
        let handle = pool.complete(fut);

        assert_eq!(handle.await_complete(), 8);
    }

    #[cfg(feature = "async")]
    #[test]
    fn test_spawn() {
        let pool = ThreadPool::default();

        async fn add(x: i32, y: i32) -> i32 {
            x + y
        }

        async fn multiply(x: i32, y: i32) -> i32 {
            x * y
        }

        let count = Arc::new(AtomicUsize::new(0));
        let clone = count.clone();
        pool.spawn(async move {
            let a = add(2, 3).await; // 5
            let b = add(2, a).await; // 7
            let c = multiply(2, b).await; // 14
            let d = multiply(a, add(2, 1).await).await; // 15
            let e = add(c, d).await; // 29

            clone.fetch_add(e as usize, Ordering::SeqCst);
        });

        pool.join();
        assert_eq!(count.load(Ordering::SeqCst), 29);
    }

    #[cfg(feature = "async")]
    #[test]
    fn test_spawn_await() {
        let pool = ThreadPool::default();

        async fn sub(x: i32, y: i32) -> i32 {
            x - y
        }

        async fn div(x: i32, y: i32) -> i32 {
            x / y
        }

        let handle = pool.spawn_await(async {
            let a = sub(120, 10).await; // 110
            let b = div(sub(a, 10).await, 4).await; // 25
            div(sub(b, div(10, 2).await).await, 5).await // 4
        });

        assert_eq!(handle.await_complete(), 4)
    }

    #[test]
    fn test_drop_oneshot_receiver() {
        let pool = Builder::new().core_size(1).max_size(1).build();

        let handle = pool.evaluate(|| {
            thread::sleep(Duration::from_secs(5));
            5
        });

        drop(handle);
        thread::sleep(Duration::from_secs(10));
        let current_thread_index = pool.worker_data.worker_number.load(Ordering::SeqCst);
        // current worker number of 2 means that one worker has started (initial number is 1 -> first worker gets and increments number)
        // indicating that the worker did not panic else it would have been replaced.
        assert_eq!(current_thread_index, 2);
    }
}
