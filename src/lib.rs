use crossbeam::{channel, crossbeam_channel};
use std::option::Option;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread;
use std::time::Duration;

type Task = Box<dyn FnOnce() + Send + 'static>;

/// This enum represents error variants that might be returned by an `execute()` invocation.
///
/// The `ChannelClosedError` wraps the `SendError` returned by crossbeam if the channel has
/// been closed unexpectedly.
///
/// The ThreadPoolClosedError is returned if `execute()` is invoked after the pool has been
/// shut down, which generally does not happen under normal circumstances since the
/// `shutdown()` function cunsumes the value.
pub enum ExecuteError<T> {
    ChannelClosedError(channel::SendError<T>),
    ThreadPoolClosedError,
}

/// Simple self growing / shrinking `ThreadPool` implementation based on crossbeam's
/// multi-producer multi-consumer channels.
///
/// This `ThreadPool` has two different pool sizes; a core pool size filled with
/// threads that live for as long as the channel and a max pool size which describes
/// the maximum amount of worker threads that may live at the sime time.
/// Those additional non-core threads have a specific keep_alive time described when
/// creating the `ThreadPool` that defines how long such threads may be idle for
/// without receiving any work before giving up and terminating their work loop.
///
/// This `ThreadPool` does not spawn any threads until a task is submitted to it.
/// Then it will create a new thread for each task until the core pool size is full.
/// After that a new thread will only be created upon an `execute()` call if the
/// current pool is lower than the max pool size and there are no idle threads.
///
/// When creating a new worker this `ThreadPool` always re-checks whether the new worker
/// is still required before spawing a thread and passing it the submitted task in case
/// an idle thread has opened up in the meantime or another thread has already created
/// the worker. If the re-check failed for a core worker the pool will try creating a
/// new non-core worker before deciding no new worker is needed.
///
/// Locks are only used for the join functions to lock the `Condvar`, apart from that
/// this `ThreadPool` implementation fully relies on crossbeam and atomic operations.
/// This `ThreadPool` decides whether it is currently idle (and should fast-return
/// join attemps) by comparing the total worker count to the idle worker count, which
/// are two `u32` values stored in one `AtomicU64` making sure that if both are updated
/// they may be updated in a single atomic operation.
///
/// The thread pool and its crossbeam channel can be destroyed by using the shutdown
/// function, however that does not stop tasks that are already running but will
/// terminate the thread the next time it will try to fetch work from the channel.
pub struct ThreadPool {
    is_shutdown: AtomicBool,
    core_size: u32,
    max_size: u32,
    keep_alive: Duration,
    worker_count_data: Arc<WorkerCountData>,
    sender: channel::Sender<Task>,
    receiver: channel::Receiver<Task>,
    join_notify_condvar: Arc<Condvar>,
    join_notify_mutex: Arc<Mutex<()>>,
}

impl ThreadPool {
    /// Construct a new ThreadPool with the specified core pool size, max pool size
    /// and keep_alive time for non-core threads. This function does not spawn any
    /// threads.
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
        let (sender, receiver) = crossbeam_channel::unbounded();

        if max_size == 0 || max_size < core_size {
            panic!("max_size must be greater than 0 and greater or equal to the core pool size");
        }

        Self {
            is_shutdown: AtomicBool::new(false),
            core_size,
            max_size,
            keep_alive,
            worker_count_data: Arc::new(WorkerCountData::default()),
            sender,
            receiver: receiver,
            join_notify_condvar: Arc::new(Condvar::new()),
            join_notify_mutex: Arc::new(Mutex::new(())),
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
        self.worker_count_data.get_total_worker_count()
    }

    /// Get the number of workers currently waiting for work. Those threads are currently
    /// polling from the crossbeam receiver. Core threads wait indefinitely and might remain
    /// in this state until the `ThreadPool` is dropped. The remaining threads give up after
    /// waiting for the specified keep_alive time.
    pub fn get_idle_worker_count(&self) -> u32 {
        self.worker_count_data.get_idle_worker_count()
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
    /// This function might panic if `try_execute` returns an error when either the crossbeam channel has been
    /// closed unexpectedly or the `execute` function was somehow invoked after the `ThreadPool` was shut down.
    /// Neither cases should occur under normal curcumstances using safe code, as shutting down the `ThreadPool`
    /// consumes ownership and the crossbeam channel is never dropped unless dropping the `ThreadPool`.
    pub fn execute<T: FnOnce() + Send + 'static>(&self, task: T) {
        if let Err(exec_err) = self.try_execute(task) {
            match exec_err {
                ExecuteError::ChannelClosedError(_) => {
                    panic!("the channel of the thread pool has been closed");
                }
                ExecuteError::ThreadPoolClosedError => {
                    panic!("the thread pool has been shut down");
                }
            }
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
    /// This function might return the `ChannelClosedError` variant of the `ExecuteError` enum
    /// if the sender was dropped unexpectedly (very unlikely) or the `ThreadPoolClosedError` variant
    /// if this function is somehow called after the `ThreadPool` has been shut down (extremely unlikely).
    pub fn try_execute<T: FnOnce() + Send + 'static>(
        &self,
        task: T,
    ) -> Result<(), ExecuteError<Box<dyn FnOnce() + Send + 'static>>> {
        if self.is_shutdown.load(Ordering::SeqCst) {
            return Err(ExecuteError::ThreadPoolClosedError);
        }

        let task: Task = Box::new(task);

        // create a new worker either if the current worker count is lower than the core pool size
        // or if there are no idle threads and the current worker count is lower than the max pool size
        let (curr_worker_count, idle_worker_count) = self.worker_count_data.get_both();
        if curr_worker_count < self.core_size {
            if let Err(task) = self.create_worker(true, task, |_, old_val| {
                old_val.0 < self.core_size && !self.is_shutdown.load(Ordering::SeqCst)
            }) {
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
    /// except if all threads are idle at the time of calling this function, in which case
    /// it will fast-return.
    ///
    /// This utilizes a `Condvar` that is notified by workers when they coplete a job and notice
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
    /// (or until the time_out is reached) except if all threads are idle at the time of calling
    /// this function, in which case it will fast-return.
    ///
    /// This utilizes a `Condvar` that is notified by workers when they coplete a job and notice
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
    pub fn shutdown(self) {
        self.is_shutdown.store(true, Ordering::SeqCst);
        drop(self);
    }

    /// Destroy this `ThreadPool` by claiming ownership and dropping the value,
    /// causing the `Sender` to drop thus disconnecting the channel.
    /// Threads in this pool that are currently executing a task will finish what
    /// they're doing until they check the channel, discovering that it has been
    /// disconnected from the sender and thus terminate their work loop.
    ///
    /// This function additionally joins all workers after dropping the pool to
    /// wait for all work to finish.
    /// Blocks the current thread until there aren't any non-idle threads anymore.
    /// This function blocks until this `ThreadPool` completes all of its work,
    /// except if all threads are idle at the time of calling this function, in which case
    /// the join will fast-return.
    ///
    /// The join utilizes a `Condvar` that is notified by workers when they coplete a job and notice
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
    /// This function additionally joins all workers after dropping the pool to
    /// wait for all work to finish.
    /// Blocks the current thread until there aren't any non-idle threads anymore or until the
    /// specified time_out Duration passes, whichever happens first.
    /// This function blocks until this `ThreadPool` completes all of its work,
    /// (or until the time_out is reached) except if all threads are idle at the time of calling
    /// this function, in which case the join will fast-return.
    ///
    /// The join utilizes a `Condvar` that is notified by workers when they coplete a job and notice
    /// that the channel is currently empty and it was the last thread to finish the current
    /// generation of work (i.e. when incrementing the idle worker counter brings the value
    /// up to the total worker counter, meaning it's the last thread to become idle).
    pub fn shutdown_join_timeout(self, timeout: Duration) {
        self.inner_shutdown_join(Some(timeout));
    }

    /// Create a new worker then check the `recheck_condition`. If this still applies give the worker
    /// the submitted task directly as its initial task. Else this method returns the task, which will
    /// be given to the main channel instead.
    fn create_worker<T: Fn(&ThreadPool, (u32, u32)) -> bool>(
        &self,
        is_core: bool,
        task: Task,
        recheck_condition: T,
    ) -> Result<(), Task> {
        let worker = Worker::new(
            self.receiver.clone(),
            Arc::clone(&self.worker_count_data),
            !is_core,
            if is_core { None } else { Some(self.keep_alive) },
            Arc::clone(&self.join_notify_condvar),
            Arc::clone(&self.join_notify_mutex),
        );

        let old_val = self.worker_count_data.increment_worker_total_ret_both();
        if recheck_condition(&self, old_val) {
            // new worker is still needed after checking again, give it the task and spawn the thread
            worker.start(task);
        } else {
            // recheck condition does not apply anymore, either there is an idle thread now (and is_core is false)
            // or the worker has already been created by another thread
            self.worker_count_data.decrement_worker_total();

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
    fn send_task_to_channel(
        &self,
        task: Task,
    ) -> Result<(), ExecuteError<Box<dyn FnOnce() + Send + 'static>>> {
        if let Err(send_err) = self.sender.send(task) {
            return Err(ExecuteError::ChannelClosedError(send_err));
        }

        Ok(())
    }

    #[inline]
    fn inner_join(&self, time_out: Option<Duration>) {
        ThreadPool::_do_join(
            self.worker_count_data.clone(),
            self.join_notify_mutex.clone(),
            self.join_notify_condvar.clone(),
            time_out,
        );
    }

    #[inline]
    fn inner_shutdown_join(self, timeout: Option<Duration>) {
        self.is_shutdown.store(true, Ordering::SeqCst);
        let current_worker_count_data = self.worker_count_data.clone();
        let mutex = self.join_notify_mutex.clone();
        let condvar = self.join_notify_condvar.clone();
        drop(self);
        ThreadPool::_do_join(current_worker_count_data, mutex, condvar, timeout);
    }

    #[inline]
    fn _do_join(
        current_worker_count_data: Arc<WorkerCountData>,
        join_notify_mutex: Arc<Mutex<()>>,
        join_notify_condvar: Arc<Condvar>,
        time_out: Option<Duration>,
    ) {
        let (current_worker_count, current_idle_count) = current_worker_count_data.get_both();
        // no thread is currently doing any work, return
        if current_idle_count == current_worker_count {
            return;
        }

        let guard = join_notify_mutex
            .lock()
            .expect("could not get join notify mutex lock");

        match time_out {
            Some(time_out) => {
                let _ret_lock = join_notify_condvar
                    .wait_timeout(guard, time_out)
                    .expect("could not wait for join condvar");
            }
            None => {
                let _ret_lock = join_notify_condvar
                    .wait(guard)
                    .expect("could not wait for join condvar");
            }
        };
    }

    fn recheck_non_core(&self, old_val: (u32, u32)) -> bool {
        let (old_worker_total, old_worker_idle) = old_val;
        old_worker_total < self.max_size
            && old_worker_idle == 0
            && !self.is_shutdown.load(Ordering::SeqCst)
    }
}

struct Worker {
    receiver: channel::Receiver<Task>,
    worker_count_data: Arc<WorkerCountData>,
    can_timeout: bool,
    keep_alive: Option<Duration>,
    join_notify_condvar: Arc<Condvar>,
    join_notify_mutex: Arc<Mutex<()>>,
}

impl Worker {
    fn new(
        receiver: channel::Receiver<Task>,
        worker_count_data: Arc<WorkerCountData>,
        can_timeout: bool,
        keep_alive: Option<Duration>,
        join_notify_condvar: Arc<Condvar>,
        join_notify_mutex: Arc<Mutex<()>>,
    ) -> Self {
        Worker {
            receiver,
            worker_count_data,
            can_timeout,
            keep_alive,
            join_notify_condvar,
            join_notify_mutex,
        }
    }

    fn start(self, task: Task) {
        thread::spawn(move || {
            self.exec_task_and_notify(task);

            loop {
                // the two functions return different error types, but since the error type doesn't matter it is mapped to unit to make them compatible
                let received_task: Result<Task, _> =
                    if self.can_timeout {
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
                        self.worker_count_data.decrement_worker_idle();
                        self.exec_task_and_notify(task);
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
            self.worker_count_data.decrement_both();
        });
    }

    #[inline]
    fn exec_task_and_notify(&self, task: Task) {
        task();
        // can already mark as idle as this thread will continue the work loop
        let (old_total_count, old_idle_count) =
            self.worker_count_data.increment_worker_idle_ret_both();
        if self.receiver.is_empty() {
            // if the last task was the last one in the current generation,
            // i.e. if incrementing the idle count lead to the idle count
            // being equal to the total worker count, notify joiners
            if old_total_count == old_idle_count + 1 {
                let _lock = self
                    .join_notify_mutex
                    .lock()
                    .expect("could not get join notify mutex lock");
                self.join_notify_condvar.notify_all();
            }
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

#[cfg(test)]
mod tests {
    use super::ThreadPool;
    use super::WorkerCountData;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::thread;
    use std::time::Duration;

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
}
