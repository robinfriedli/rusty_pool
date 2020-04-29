use crossbeam::{channel, crossbeam_channel};
use std::option::Option;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Condvar, Mutex,
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

/// Simple self growing / shrinking `ThreadPool` implementation based on crossbeam's multi-producer multi-consumer channels.
///
/// This `ThreadPool` has two different pool sizes; a core pool size filled with threads that live for as long as the channel
/// and a max pool size which describes the maximum amount of worker threads that may live at the sime time.
/// Those additional non-core threads have a specific keep_alive time described when creating the `ThreadPool` that defines
/// how long such threads may be idle for without receiving any work before giving up and terminating their work loop.
///
/// This `ThreadPool` does not spawn any threads until a task is submitted to it. Then it will create a new thread for each task
/// until the core pool size is full. After that a new thread will only be created upon an `execute()` call if the current pool
/// is lower than the max pool size and there are no idle threads.
///
/// When creating a new worker thread this `ThreadPool` always checks whether the new worker is still required before starting the
/// worker's main work queue in case an idle thread has opened up in the meantime or another thread has already created the worker.
///
/// Locks are only used for the join functions to lock the `Condvar`, apart from that this `ThreadPool` implementation fully relies
/// on crossbeam and atomic operations.
///
/// The thread pool and its crossbeam channel can be destroyed by using the shutdown function, however that does not
/// stop tasks that are already running but will terminate the thread the next time it will try to fetch work from the channel.
pub struct ThreadPool {
    is_shutdown: AtomicBool,
    core_size: usize,
    max_size: usize,
    keep_alive: Duration,
    worker_count: Arc<AtomicUsize>,
    idle_worker_count: Arc<AtomicUsize>,
    sender: channel::Sender<Task>,
    receiver: Arc<channel::Receiver<Task>>,
    join_notify_condvar: Arc<Condvar>,
    join_notify_mutex: Arc<Mutex<()>>,
}

impl ThreadPool {
    /// Construct a new ThreadPool with the specified core pool size, max pool size and keep_alive time for non-core threads.
    /// This function does not spawn any threads
    ///
    /// # Panics
    ///
    /// This function will panic if max_size is 0 or lower than core_size.
    pub fn new(core_size: usize, max_size: usize, keep_alive: Duration) -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();

        if max_size == 0 || max_size < core_size {
            panic!("max_size must be greater than 0 and greater or equal to the core pool size");
        }

        Self {
            is_shutdown: AtomicBool::new(false),
            core_size,
            max_size,
            keep_alive,
            worker_count: Arc::new(AtomicUsize::new(0)),
            idle_worker_count: Arc::new(AtomicUsize::new(0)),
            sender,
            receiver: Arc::new(receiver),
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
    /// and the worker will never start its working loop. Else this counter is decremented when a
    /// worker reaches the end of its working loop, which for non-core threads might happen
    /// if it does not receive any work during its keep alive time, for core threads this only
    /// happens once the channel is disconnected.
    pub fn get_current_worker_count(&self) -> usize {
        self.worker_count.load(Ordering::SeqCst)
    }

    /// Get the number of workers currently waiting for work. Those threads are currently
    /// polling from the crossbeam receiver. Core threads wait indefinitely and might remain
    /// in this state until the `ThreadPool` is dropped. The remaining threads give up after
    /// waiting for the specified keep_alive time.
    pub fn get_idle_worker_count(&self) -> usize {
        self.idle_worker_count.load(Ordering::SeqCst)
    }

    /// Send a new task to the worker threads. This function is responsible for sending the message through the
    /// channel and creating new workers if needed. If the current worker count is lower than the core pool size
    /// this function will always create a new worker. If the current worker count is equal to or greater than
    /// the core pool size this function only creates a new worker if the worker count is below the max pool size
    /// and there are no idle threads.
    ///
    /// After constructing the worker but before starting its work loop this function checks again whether the new
    /// worker is still needed by analysing the old value returned by atomically incrementing the worker counter
    /// and checking if the worker is still needed or if another thread has already created it,
    /// for non-core threads this additionally checks whether there are idle threads now.
    ///
    /// # Errors
    ///
    /// This function might return the `ChannelClosedError` variant of the `ExecuteError` enum
    /// if the sender bas dropped unexpectedly (very unlikely) or the `ThreadPoolClosedError` variant
    /// if this function is somehow called after the `ThreadPool` has been shut down (extremely unlikely).
    pub fn execute<T: FnOnce() + Send + 'static>(
        &self,
        task: T,
    ) -> Result<(), ExecuteError<Box<dyn FnOnce() + Send + 'static>>> {
        if self.is_shutdown.load(Ordering::SeqCst) {
            return Err(ExecuteError::ThreadPoolClosedError);
        }

        // early exit if channel has already been closed
        if let Err(send_err) = self.sender.send(Box::new(task)) {
            return Err(ExecuteError::ChannelClosedError(send_err));
        }

        // create a new worker either if the current worker count is lower than the core pool size
        // or if there are no idle threads and the current worker count is lower than the max pool size
        let curr_worker_count = self.worker_count.load(Ordering::SeqCst);
        if curr_worker_count < self.core_size {
            self.create_worker(true, |old_val| {
                old_val < self.core_size && !self.is_shutdown.load(Ordering::SeqCst)
            });
        } else if curr_worker_count < self.max_size
            && self.idle_worker_count.load(Ordering::SeqCst) == 0
        {
            self.create_worker(false, |old_val| {
                old_val < self.max_size
                    && self.idle_worker_count.load(Ordering::SeqCst) == 0
                    && !self.is_shutdown.load(Ordering::SeqCst)
            })
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
    /// that the channel is currently empty.
    pub fn join(&self) {
        self.join_internal(None);
    }

    /// Blocks the current thread until there aren't any non-idle threads anymore or until the
    /// specified time_out Duration passes, whichever happens first.
    /// This includes work started after calling this function.
    /// This function blocks until the next time this `ThreadPool` completes all of its work,
    /// (or until the time_out is reached) except if all threads are idle at the time of calling
    /// this function, in which case it will fast-return.
    ///
    /// This utilizes a `Condvar` that is notified by workers when they coplete a job and notice
    /// that the channel is currently empty.
    pub fn join_timeout(&self, time_out: Duration) {
        self.join_internal(Some(time_out));
    }

    /// Destroy this ThreadPool and consume the value associated with it.
    /// Threads in this pool that are currently executing a task will finish what
    /// they're doing until they check the channel, discovering that it has been
    /// disconnected from the sender and thus terminate their work loop,
    pub fn shutdown(self) {
        self.is_shutdown.store(true, Ordering::SeqCst);
        drop(self);
    }

    fn create_worker<T: FnOnce(usize) -> bool>(&self, is_core: bool, recheck_condition: T) {
        let (green_light_sender, green_light_receiver) = mpsc::channel();
        Worker::new(
            Arc::clone(&self.receiver),
            green_light_receiver,
            Arc::clone(&self.worker_count),
            Arc::clone(&self.idle_worker_count),
            !is_core,
            if is_core { None } else { Some(self.keep_alive) },
            Arc::clone(&self.join_notify_condvar),
            Arc::clone(&self.join_notify_mutex),
        );

        let old_val = self.worker_count.fetch_add(1, Ordering::SeqCst);
        if recheck_condition(old_val) {
            // new worker is still needed after checking again, give it the green light
            green_light_sender
                .send(true)
                .expect("failed to send green light signal to worker");
        } else {
            // recheck condition does not apply anymore, either there is an idle thread now (and is_core is false)
            // or the worker has already been created by another thread
            green_light_sender
                .send(false)
                .expect("failed to send denied green light signal to worker");
            self.worker_count.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn join_internal(&self, time_out: Option<Duration>) {
        let current_worker_count = self.worker_count.load(Ordering::SeqCst);
        let current_idle_count = self.idle_worker_count.load(Ordering::SeqCst);

        // no thread is currently doing any work, return
        if current_idle_count == current_worker_count {
            return;
        }

        let guard = self
            .join_notify_mutex
            .lock()
            .expect("could not get join notify mutex lock");

        match time_out {
            Some(time_out) => {
                let _ret_lock = self
                    .join_notify_condvar
                    .wait_timeout(guard, time_out)
                    .expect("could not wait for join condvar");
            }
            None => {
                let _ret_lock = self
                    .join_notify_condvar
                    .wait(guard)
                    .expect("could not wait for join condvar");
            }
        };
    }
}

struct Worker;

impl Worker {
    fn new(
        receiver: Arc<channel::Receiver<Task>>,
        green_light_receiver: mpsc::Receiver<bool>,
        worker_count: Arc<AtomicUsize>,
        idle_worker_count: Arc<AtomicUsize>,
        can_timeout: bool,
        keep_alive: Option<Duration>,
        join_notify_condvar: Arc<Condvar>,
        join_notify_mutex: Arc<Mutex<()>>,
    ) -> Self {
        thread::spawn(move || {
            match green_light_receiver.recv() {
                Ok(true) => {
                    // received green light, continue
                }
                _ => {
                    // either the channel was disconnected or green light was not given,
                    // indicating that the worker was already created by an other thread
                    // and is now obsolete
                    return;
                }
            }

            // mark current thread as idle
            idle_worker_count.fetch_add(1, Ordering::SeqCst);
            loop {
                // the two functions return different error types, but since the error type doesn't matter it is mapped to unit to make them compatible
                let received_task: Result<Task, _> =
                    if can_timeout {
                        receiver
                            .recv_timeout(keep_alive.expect(
                                "keep_alive duration is NONE despite can_timeout being true",
                            ))
                            .map_err(|_| ())
                    } else {
                        receiver.recv().map_err(|_| ())
                    };

                match received_task {
                    Ok(task) => {
                        // mark current as no longer idle and execute task
                        idle_worker_count.fetch_sub(1, Ordering::SeqCst);
                        task();
                        idle_worker_count.fetch_add(1, Ordering::SeqCst);
                        if receiver.is_empty() {
                            let _lock = join_notify_mutex
                                .lock()
                                .expect("could not get join notify mutex lock");
                            join_notify_condvar.notify_all();
                        }
                    }
                    Err(_) => {
                        // either channel was broken because the sender disconnected or, if can_timeout is true, the Worker has not received any work during
                        // its keep_alive period and will now terminate, break working loop
                        idle_worker_count.fetch_sub(1, Ordering::SeqCst);
                        break;
                    }
                }
            }
            worker_count.fetch_sub(1, Ordering::SeqCst);
        });

        Worker
    }
}

#[allow(unused_must_use)]
#[cfg(test)]
mod tests {
    use super::ThreadPool;
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
        let pool = ThreadPool::new(3, 50, Duration::from_secs(30));
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..160 {
            let clone = counter.clone();
            pool.execute(move || {
                clone.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_secs(10))
            });
        }

        thread::sleep(Duration::from_secs(5));
        assert!(pool.get_current_worker_count() <= 50);
        thread::sleep(Duration::from_secs(20));

        for _ in 0..160 {
            let clone = counter.clone();
            pool.execute(move || {
                clone.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_secs(10))
            });
        }

        thread::sleep(Duration::from_secs(5));
        assert!(pool.get_current_worker_count() <= 50);
        thread::sleep(Duration::from_secs(200));

        assert_eq!(counter.load(Ordering::SeqCst), 320);
        assert_eq!(pool.get_current_worker_count(), 3);
        assert_eq!(pool.get_idle_worker_count(), 3);
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
}
