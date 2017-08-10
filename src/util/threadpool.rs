// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::usize;
use std::sync::{Arc, Mutex, Condvar};
use std::thread::{Builder, JoinHandle};
use std::boxed::FnBox;
use std::collections::VecDeque;
use std::cmp::Ordering;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::hash::Hash;
use std::marker::PhantomData;
use std::fmt::{self, Write, Debug, Formatter};
use std::sync::mpsc::{Sender, Receiver, channel};

const DEFAULT_QUEUE_CAPACITY: usize = 1000;
const QUEUE_MAX_CAPACITY: usize = 8 * DEFAULT_QUEUE_CAPACITY;

pub struct Task<T, C> {
    // The task's id in the pool. Each task has a unique id,
    // and it's always bigger than preceding ones.
    id: u64,

    // which group the task belongs to.
    gid: T,
    task: Box<FnBox(C) + Send>,
    ctx: C,
}

impl<T: Debug, C> Debug for Task<T, C> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "task_id:{},group_id:{:?}", self.id, self.gid)
    }
}


impl<T, C: Context> Task<T, C> {
    fn new<F>(gid: T, job: F, ctx: C) -> Task<T, C>
        where F: FnOnce(C) + Send + 'static
    {
        Task {
            id: 0,
            gid: gid,
            task: Box::new(job),
            ctx: ctx,
        }
    }
}

impl<T, C> Ord for Task<T, C> {
    fn cmp(&self, right: &Task<T, C>) -> Ordering {
        self.id.cmp(&right.id).reverse()
    }
}

impl<T, C> PartialEq for Task<T, C> {
    fn eq(&self, right: &Task<T, C>) -> bool {
        self.cmp(right) == Ordering::Equal
    }
}

impl<T, C> Eq for Task<T, C> {}

impl<T, C> PartialOrd for Task<T, C> {
    fn partial_cmp(&self, rhs: &Task<T, C>) -> Option<Ordering> {
        Some(self.cmp(rhs))
    }
}

pub trait ScheduleQueue<T: Debug, C> {
    fn pop(&mut self) -> Option<Task<T, C>>;
    fn push(&mut self, task: Task<T, C>);
    fn on_task_finished(&mut self, gid: &T);
    fn on_task_started(&mut self, gid: &T);
}

// First in first out queue.
pub struct FifoQueue<T, C> {
    queue: VecDeque<Task<T, C>>,
}

impl<T: Hash + Ord + Send + Clone + Debug, C: Context> FifoQueue<T, C> {
    pub fn new() -> FifoQueue<T, C> {
        FifoQueue { queue: VecDeque::with_capacity(DEFAULT_QUEUE_CAPACITY) }
    }
}

impl<T: Hash + Ord + Send + Clone + Debug, C> ScheduleQueue<T, C> for FifoQueue<T, C> {
    fn push(&mut self, task: Task<T, C>) {
        self.queue.push_back(task);
    }

    fn pop(&mut self) -> Option<Task<T, C>> {
        let task = self.queue.pop_front();

        if self.queue.is_empty() && self.queue.capacity() > QUEUE_MAX_CAPACITY {
            self.queue = VecDeque::with_capacity(DEFAULT_QUEUE_CAPACITY);
        }

        task
    }

    fn on_task_started(&mut self, _: &T) {}
    fn on_task_finished(&mut self, _: &T) {}
}

struct TaskPool<Q, T, C> {
    next_task_id: u64,
    task_queue: Q,
    marker: PhantomData<T>,
    stop: bool,
    jobs: Receiver<Task<T, C>>,
}

impl<Q, T, C> TaskPool<Q, T, C>
    where Q: ScheduleQueue<T, C>,
          T: Debug,
          C: Context
{
    fn new(queue: Q, jobs: Receiver<Task<T, C>>) -> TaskPool<Q, T, C> {
        TaskPool {
            next_task_id: 0,
            task_queue: queue,
            marker: PhantomData,
            stop: false,
            jobs: jobs,
        }
    }

    fn on_task_finished(&mut self, gid: &T) {
        self.task_queue.on_task_finished(gid);
    }

    fn on_task_started(&mut self, gid: &T) {
        self.task_queue.on_task_started(gid);
    }

    fn pop_task(&mut self) -> Option<Task<T, C>> {
        if let Some(task) = self.task_queue.pop() {
            return Some(task);
        }
        // try fill queue when queue is empty.
        self.try_fill_queue();
        self.task_queue.pop()
    }

    fn try_fill_queue(&mut self) {
        while let Ok(mut task) = self.jobs.try_recv() {
            task.id = self.next_task_id;
            self.next_task_id += 1;
            self.task_queue.push(task);
        }
    }

    #[inline]
    fn stop(&mut self) {
        self.stop = true;
    }

    #[inline]
    fn is_stopped(&self) -> bool {
        self.stop
    }
}

pub trait Context: Send + Clone {
    fn set(&mut self, key: &str, value: u64);
    fn get(&self, key: &str) -> u64;
    fn on_start(&self);
    fn on_complete(&self);
}

pub trait ContextFactory<Ctx: Context> {
    fn create_context(&self) -> Ctx;
}

/// `ThreadPool` is used to execute tasks in parallel.
/// Each task would be pushed into the pool, and when a thread
/// is ready to process a task, it will get a task from the pool
/// according to the `ScheduleQueue` provided in initialization.
pub struct ThreadPool<Q, T, C, Ctx> {
    task_pool: Arc<(Mutex<TaskPool<Q, T, Ctx>>, Condvar)>,
    threads: Vec<JoinHandle<()>>,
    task_count: Arc<AtomicUsize>,
    sender: Sender<Task<T, Ctx>>,
    // ctx_factory should only be used in one thread
    ctx_factory: C,
}

impl<Q, T, C, Ctx> ThreadPool<Q, T, C, Ctx>
    where Q: ScheduleQueue<T, Ctx> + Send + 'static,
          T: Hash + Send + Clone + 'static + Debug,
          Ctx: Context + 'static,
          C: ContextFactory<Ctx>
{
    pub fn new(name: String, num_threads: usize, queue: Q, f: C) -> ThreadPool<Q, T, C, Ctx> {
        assert!(num_threads >= 1);
        let (sender, receiver) = channel::<Task<T, Ctx>>();
        let task_pool = Arc::new((Mutex::new(TaskPool::new(queue, receiver)), Condvar::new()));
        let mut threads = Vec::with_capacity(num_threads);
        let task_count = Arc::new(AtomicUsize::new(0));
        // Threadpool threads
        for _ in 0..num_threads {
            let tasks = task_pool.clone();
            let task_num = task_count.clone();
            let thread = Builder::new()
                .name(name.clone())
                .spawn(move || {
                    let mut worker = Worker::new(tasks, task_num);
                    worker.run();
                })
                .unwrap();
            threads.push(thread);
        }

        ThreadPool {
            task_pool: task_pool,
            threads: threads,
            task_count: task_count,
            sender: sender,
            ctx_factory: f,
        }
    }

    pub fn execute<F>(&mut self, gid: T, job: F)
        where F: FnOnce(Ctx) + Send + 'static,
              Ctx: Context
    {
        let ctx = self.ctx_factory.create_context();
        let task = Task::new(gid, job, ctx);
        self.sender.send(task).unwrap();
        self.task_count.fetch_add(1, AtomicOrdering::SeqCst);
        let &(_, ref cvar) = &*self.task_pool;
        cvar.notify_one();
    }

    #[inline]
    pub fn get_task_count(&self) -> usize {
        self.task_count.load(AtomicOrdering::SeqCst)
    }

    pub fn stop(&mut self) -> Result<(), String> {
        {
            let &(ref lock, ref cvar) = &*self.task_pool;
            let mut tasks = lock.lock().unwrap();
            tasks.stop();
            cvar.notify_all();
        }
        let mut err_msg = String::new();
        for t in self.threads.drain(..) {
            if let Err(e) = t.join() {
                write!(&mut err_msg, "Failed to join thread with err: {:?};", e).unwrap();
            }
        }
        if !err_msg.is_empty() {
            return Err(err_msg);
        }
        Ok(())
    }
}

// Each thread has a worker.
struct Worker<Q, T, C> {
    task_pool: Arc<(Mutex<TaskPool<Q, T, C>>, Condvar)>,
    task_count: Arc<AtomicUsize>,
}

impl<Q, T, C> Worker<Q, T, C>
    where Q: ScheduleQueue<T, C>,
          T: Debug,
          C: Context
{
    fn new(task_pool: Arc<(Mutex<TaskPool<Q, T, C>>, Condvar)>,
           task_count: Arc<AtomicUsize>)
           -> Worker<Q, T, C> {
        Worker {
            task_pool: task_pool,
            task_count: task_count,
        }
    }

    // `get_next_task` return `None` when `task_pool` is stopped.
    #[inline]
    fn get_next_task(&self, prev_gid: Option<&T>) -> Option<Task<T, C>> {
        // try to receive notification.
        let &(ref lock, ref cvar) = &*self.task_pool;
        let mut task_pool = lock.lock().unwrap();
        if prev_gid.is_some() {
            task_pool.on_task_finished(prev_gid.unwrap());
        }
        loop {
            if task_pool.is_stopped() {
                return None;
            }
            if let Some(task) = task_pool.pop_task() {
                // `on_task_started` should be here since:
                //  1. To reduce lock's time;
                //  2. For some schedula_queue,on_task_started should be
                //  in the same lock with `pop_task` for the thread safety.
                task_pool.on_task_started(&task.gid);
                return Some(task);
            }
            task_pool = cvar.wait(task_pool).unwrap();
        }
    }

    fn run(&mut self) {
        let mut task = self.get_next_task(None);
        // Start the worker. Loop breaks when receive stop message.
        while let Some(t) = task {
            t.ctx.on_start();
            // Since tikv would be down when any panic happens,
            // we don't need to process panic case here.
            (t.task)(t.ctx.clone());
            t.ctx.on_complete();
            self.task_count.fetch_sub(1, AtomicOrdering::SeqCst);
            task = self.get_next_task(Some(&t.gid));
        }
    }
}

#[cfg(test)]
mod test {
    use super::{ThreadPool, Task, ScheduleQueue, FifoQueue, Context, ContextFactory};
    use std::time::Duration;
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct DummyContext {}

    unsafe impl Send for DummyContext {}

    impl Context for DummyContext {
        fn get(&self, _: &str) -> u64 {
            1
        }
        fn set(&mut self, _: &str, _: u64) {}
        fn on_start(&self) {}
        fn on_complete(&self) {}
    }

    struct DummyContextFactory {}

    impl ContextFactory<DummyContext> for DummyContextFactory {
        fn create_context(&self) -> DummyContext {
            DummyContext {}
        }
    }

    #[test]
    fn test_for_tasks_with_different_cost() {
        let name = thd_name!("test_tasks_with_different_cost");
        let concurrency = 2;
        let f = DummyContextFactory {};
        let mut task_pool = ThreadPool::new(name, concurrency, FifoQueue::new(), f);
        let (jtx, jrx) = channel();
        let group_with_big_task = 1001 as u64;
        let timeout = Duration::from_secs(2);
        let (ftx, frx) = channel();
        // Push a big task into pool.
        task_pool.execute(group_with_big_task, move |_: DummyContext| {
            // Since a long task of `group_with_big_task` is running,
            // the other threads shouldn't run any task of `group_with_big_task`.
            for _ in 0..10 {
                let gid = jrx.recv_timeout(timeout).unwrap();
                assert_ne!(gid, group_with_big_task);
            }
            for _ in 0..10 {
                let gid = jrx.recv_timeout(timeout).unwrap();
                assert_eq!(gid, group_with_big_task);
            }
            ftx.send(true).unwrap();
        });

        for gid in 0..10 {
            let sender = jtx.clone();
            task_pool.execute(gid, move |_: DummyContext| {
                sender.send(gid).unwrap();
            });
        }

        for _ in 0..10 {
            let sender = jtx.clone();
            task_pool.execute(group_with_big_task, move |_: DummyContext| {
                sender.send(group_with_big_task).unwrap();
            });
        }
        frx.recv_timeout(timeout).unwrap();
        task_pool.stop().unwrap();
    }

    #[test]
    fn test_get_task_count() {
        let name = thd_name!("test_get_task_count");
        let concurrency = 1;
        let f = DummyContextFactory {};
        let mut task_pool = ThreadPool::new(name, concurrency, FifoQueue::new(), f);
        let (tx, rx) = channel();
        let (ftx, frx) = channel();
        let receiver = Arc::new(Mutex::new(rx));
        let timeout = Duration::from_secs(2);
        let group_num = 4;
        let mut task_num = 0;
        for gid in 0..group_num {
            let rxer = receiver.clone();
            let ftx = ftx.clone();
            task_pool.execute(gid, move |_: DummyContext| {
                let rx = rxer.lock().unwrap();
                let id = rx.recv_timeout(timeout).unwrap();
                assert_eq!(id, gid);
                ftx.send(true).unwrap();
            });
            task_num += 1;
            assert_eq!(task_pool.get_task_count(), task_num);
        }

        for gid in 0..group_num {
            tx.send(gid).unwrap();
            frx.recv_timeout(timeout).unwrap();
            let left_num = task_pool.get_task_count();
            // current task may be still running.
            assert!(left_num == task_num || left_num == task_num - 1,
                    format!("left_num {},task_num {}", left_num, task_num));
            task_num -= 1;
        }
        task_pool.stop().unwrap();
    }

    #[test]
    fn test_fifo_queue() {
        let mut queue = FifoQueue::new();
        let f = DummyContextFactory {};
        for id in 0..10 {
            let mut task = Task::new(0, move |_: DummyContext| {}, f.create_context());
            task.id = id;
            queue.push(task);
        }
        for id in 0..10 {
            let task = queue.pop().unwrap();
            assert_eq!(id, task.id);
        }
    }
}
