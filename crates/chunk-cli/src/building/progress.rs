use std::{sync::Arc, time::Duration};

#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Compile,
    Release,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Self::Compile => "Compile",
            Self::Release => "Release",
        }
    }
}

pub(crate) enum Event {
    Started(Phase),
    Finished(Phase, Duration),
    Output(String),
}

/// Optional observer shared by the build and its output readers.
#[derive(Clone, Default)]
pub(crate) struct Progress(Option<Arc<dyn Fn(Event) + Send + Sync>>);

impl Progress {
    pub fn new(observer: impl Fn(Event) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(observer)))
    }

    pub fn emit(&self, event: Event) {
        if let Some(observer) = &self.0 {
            observer(event);
        }
    }
}
