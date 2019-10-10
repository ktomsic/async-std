use std::cell::UnsafeCell;
use std::mem::{self, ManuallyDrop};
use std::panic::{self, AssertUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{RawWaker, RawWakerVTable};
use std::thread::{self, Thread};

use crate::future::Future;
use crate::task::task;
use crate::task::task_local;
use crate::task::worker;
use crate::task::{Context, Poll, Waker};

use kv_log_macro::trace;

/// Spawns a task and blocks the current thread on its result.
///
/// Calling this function is similar to [spawning] a thread and immediately [joining] it, except an
/// asynchronous task will be spawned.
///
/// See also: [`task::blocking`].
///
/// [`task::blocking`]: ../task/fn.blocking.html
///
/// [spawning]: https://doc.rust-lang.org/std/thread/fn.spawn.html
/// [joining]: https://doc.rust-lang.org/std/thread/struct.JoinHandle.html#method.join
///
/// # Examples
///
/// ```no_run
/// use async_std::thread;
///
/// fn main() {
///     thread::spawn_task(async {
///         println!("Hello, world!");
///     })
/// }
/// ```
pub fn spawn_task<F, T>(future: F) -> T
where
    F: Future<Output = T>,
    T: Send,
{
    unsafe {
        // A place on the stack where the result will be stored.
        let out = &mut UnsafeCell::new(None);

        // Wrap the future into one that stores the result into `out`.
        let future = {
            let out = out.get();

            async move {
                let future = CatchUnwindFuture {
                    future: AssertUnwindSafe(future),
                };
                *out = Some(future.await);
            }
        };

        // Create a tag for the task.
        let tag = task::Tag::new(None);

        // Log this `spawn_task` operation.
        let child_id = tag.task_id().as_u64();
        let parent_id = worker::get_task(|t| t.id().as_u64()).unwrap_or(0);

        trace!("spawn_task", {
            parent_id: parent_id,
            child_id: child_id,
        });

        // Wrap the future into one that drops task-local variables on exit.
        let future = task_local::add_finalizer(future);

        let future = async move {
            future.await;
            trace!("spawn_task completed", {
                parent_id: parent_id,
                child_id: child_id,
            });
        };

        // Pin the future onto the stack.
        pin_utils::pin_mut!(future);

        // Transmute the future into one that is futurestatic.
        let future = mem::transmute::<
            Pin<&'_ mut dyn Future<Output = ()>>,
            Pin<&'static mut dyn Future<Output = ()>>,
        >(future);

        // Block on the future and and wait for it to complete.
        worker::set_tag(&tag, || block(future));

        // Take out the result.
        match (*out.get()).take().unwrap() {
            Ok(v) => v,
            Err(err) => panic::resume_unwind(err),
        }
    }
}

struct CatchUnwindFuture<F> {
    future: F,
}

impl<F> CatchUnwindFuture<F> {
    pin_utils::unsafe_pinned!(future: F);
}

impl<F: Future + UnwindSafe> Future for CatchUnwindFuture<F> {
    type Output = thread::Result<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        panic::catch_unwind(AssertUnwindSafe(|| self.future().poll(cx)))?.map(Ok)
    }
}

fn block<F, T>(f: F) -> T
where
    F: Future<Output = T>,
{
    thread_local! {
        static ARC_THREAD: Arc<Thread> = Arc::new(thread::current());
    }

    pin_utils::pin_mut!(f);

    ARC_THREAD.with(|arc_thread: &Arc<Thread>| {
        let ptr = (&**arc_thread as *const Thread) as *const ();
        let vt = vtable();

        let waker = unsafe { ManuallyDrop::new(Waker::from_raw(RawWaker::new(ptr, vt))) };
        let cx = &mut Context::from_waker(&waker);

        loop {
            if let Poll::Ready(t) = f.as_mut().poll(cx) {
                return t;
            }
            thread::park();
        }
    })
}

fn vtable() -> &'static RawWakerVTable {
    unsafe fn clone_raw(ptr: *const ()) -> RawWaker {
        let arc = ManuallyDrop::new(Arc::from_raw(ptr as *const Thread));
        mem::forget(arc.clone());
        RawWaker::new(ptr, vtable())
    }

    unsafe fn wake_raw(ptr: *const ()) {
        let arc = Arc::from_raw(ptr as *const Thread);
        arc.unpark();
    }

    unsafe fn wake_by_ref_raw(ptr: *const ()) {
        let arc = ManuallyDrop::new(Arc::from_raw(ptr as *const Thread));
        arc.unpark();
    }

    unsafe fn drop_raw(ptr: *const ()) {
        drop(Arc::from_raw(ptr as *const Thread))
    }

    &RawWakerVTable::new(clone_raw, wake_raw, wake_by_ref_raw, drop_raw)
}