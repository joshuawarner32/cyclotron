use std::fmt::Debug;
use std::mem;
use std::ops::{
    Deref,
    DerefMut,
};
use std::sync::{Arc, Mutex};
use futures::{
    Async,
    Future,
    Poll,
};
use futures::task::{
    self,
    Task,
};
use futures::executor::{Notify, NotifyHandle, spawn};
use event::{AsyncOutcome, SpanId, TraceEvent};
use state::TRACER_STATE;

/// Atomic slot of a single parked task.  Note that this only parks at most one
/// task: If your data-structure needs to wakeup potentially many threads, using
/// `TaskSlot` will cause a deadlock.  Compare `FutureSet` with `TicketMaster`
/// for an example.
#[derive(Default)]
pub struct AtomicTask {
    task: Mutex<Option<Task>>,
}

impl AtomicTask {
    pub fn notify(&self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.notify();
        }
    }

    pub fn park(&self) {
        let mut task = self.task.lock().unwrap();
        if let Some(ref task) = *task {
            if task.will_notify_current() {
                return;
            }
        }
        *task = Some(task::current());
    }
}

pub trait TraceFuture: Future + Sized where Self::Error : Debug {
    fn traced<S: Into<String>>(self, name: S) -> TracedFuture<Self> {
        TracedFuture {
            state: TraceState::Created { name: name.into() },
            inner: self,
        }
    }
}
impl<F: Future + Sized> TraceFuture for F where F::Error : Debug {}

enum TraceState {
    Created {
        name: String,
    },
    Executing {
        parent: SpanId,
        id: SpanId,
    },
    Resolved,
    Poisoned,
}

pub struct TracedFuture<F> {
    state: TraceState,
    inner: F,
}

impl<F> Deref for TracedFuture<F> {
    type Target = F;
    fn deref(&self) -> &F {
        &self.inner
    }
}

impl<F> DerefMut for TracedFuture<F> {
    fn deref_mut(&mut self) -> &mut F {
        &mut self.inner
    }
}

impl<F> TracedFuture<F> {
    pub fn into_inner(self) -> F {
        self.inner
    }
}

impl<F: Future> Future for TracedFuture<F> where F::Error : Debug {
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<F::Item, F::Error> {
        TRACER_STATE.with(|c| {
            let (parent_id, span_id) = {
                let mut st = c.borrow_mut();
                let (parent_id, span_id) = match mem::replace(&mut self.state, TraceState::Poisoned) {
                    // First poll!  Let's set up our execution state.
                    TraceState::Created { name } => {
                        let span_id = SpanId::new();
                        let parent_id = st.current_span.expect("Missing parent span");

                        let event = TraceEvent::AsyncStart {
                            name: name,
                            id: span_id,
                            parent_id: parent_id,
                            ts: st.now(),
                        };
                        st.emit(event);

                        self.state = TraceState::Executing {
                            parent: parent_id,
                            id: span_id,
                        };
                        (parent_id, span_id)
                    },
                    TraceState::Executing { parent, id } => {
                        assert_eq!(st.current_span, Some(parent), "Parent span changed across execution");
                        self.state = TraceState::Executing { parent, id };
                        (parent, id)
                    },
                    TraceState::Resolved => panic!("Polled after resolved"),
                    TraceState::Poisoned => panic!("Polled after panic"),
                };

                let on_event = TraceEvent::AsyncOnCPU {
                    id: span_id,
                    ts: st.now(),
                };
                st.emit(on_event);
                st.current_span = Some(span_id);

                (parent_id, span_id)
            };

            let notifier = Notifier { parent_task: AtomicTask::default(), parked_span: span_id };
            notifier.parent_task.park();
            let handle = NotifyHandle::from(Arc::new(notifier));

            let result = {
                let mut f = spawn(&mut self.inner);
                f.poll_future_notify(&handle, 0)
            };

            let mut st = c.borrow_mut();

            st.current_span = Some(parent_id);
            let off_event = TraceEvent::AsyncOffCPU {
                id: span_id,
                ts: st.now(),
            };
            st.emit(off_event);

            match result {
                Ok(Async::Ready(..)) => {
                    self.state = TraceState::Resolved;
                    let end_event = TraceEvent::AsyncEnd {
                        id: span_id,
                        ts: st.now(),
                        outcome: AsyncOutcome::Success,
                    };
                    st.emit(end_event);
                },
                Err(ref e) => {
                    self.state = TraceState::Resolved;
                    let end_event = TraceEvent::AsyncEnd {
                        id: span_id,
                        ts: st.now(),
                        outcome: AsyncOutcome::Error(format!("{:?}", e)),
                    };
                    st.emit(end_event);
                },
                Ok(Async::NotReady) => (),
            }
            result
        })
    }
}

struct Notifier {
    parent_task: AtomicTask,
    parked_span: SpanId,
}

impl Notify for Notifier {
    fn notify(&self, _: usize) {
        TRACER_STATE.with(|c| {
            let should_log = {
                let mut st = c.borrow_mut();
                let should_log = !st.currently_logging_wakeup;
                if should_log {
                    if let Some(current_span) = st.current_span {
                        let event = TraceEvent::Wakeup {
                            waking_span: current_span,
                            parked_span: self.parked_span,
                            ts: st.now(),
                        };
                        st.emit(event);
                    }
                    st.currently_logging_wakeup = true;
                }
                should_log
            };

            self.parent_task.notify();

            if should_log {
                let mut st = c.borrow_mut();
                st.currently_logging_wakeup = false;
            }
        })
    }
}
