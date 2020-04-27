use event::{SpanId, TraceEvent};
use state::{TRACER_STATE, Logger};

pub struct TracedThread {
    id: SpanId,
}

impl TracedThread {
    pub fn new<S: Into<String>>(name: S, writer: Box<dyn Logger>) -> Self {
        TRACER_STATE.with(|c| {
            let mut st = c.borrow_mut();
            st.start(writer);
            let span_id = SpanId::new();

            assert!(st.current_span.is_none());
            st.current_span = Some(span_id);

            let event = TraceEvent::ThreadStart {
                name: name.into(),
                id: span_id,
                ts: st.now(),
            };
            st.emit(event);

            TracedThread { id: span_id }
        })
    }
}

impl Drop for TracedThread {
    fn drop(&mut self) {
        TRACER_STATE.with(|c| {
            let mut st = c.borrow_mut();
            st.current_span = None;

            let event = TraceEvent::ThreadEnd {
                id: self.id,
                ts: st.now(),
            };
            st.emit(event);
        })
    }
}

pub struct SyncSpan {
    parent: SpanId,
    id: SpanId,
}

impl SyncSpan {
    pub fn new<S: Into<String>>(name: S) -> Self {
        TRACER_STATE.with(|c| {
            let mut st = c.borrow_mut();

            let span_id = SpanId::new();
            let parent_id = st.current_span.take().expect("Missing parent span");
            st.current_span = Some(span_id);

            let event = TraceEvent::SyncStart {
                name: name.into(),
                id: span_id,
                parent_id: parent_id,
                ts: st.now(),
            };
            st.emit(event);

            SyncSpan {
                parent: parent_id,
                id: span_id,
            }
        })
    }
}

impl Drop for SyncSpan {
    fn drop(&mut self) {
        TRACER_STATE.with(|c| {
            let mut st = c.borrow_mut();
            assert_eq!(st.current_span, Some(self.id), "Current span changed during SyncSpan");
            st.current_span = Some(self.parent);

            let event = TraceEvent::SyncEnd {
                id: self.id,
                ts: st.now(),
            };
            st.emit(event);
        })
    }
}
