use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use tracing::Span;

#[derive(Clone)]
pub struct ThreadPool {
    tx: crossbeam_channel::Sender<(Span, Box<dyn FnOnce() + Send + 'static>)>,
    active_count: Arc<AtomicUsize>,
    completion_pair: Arc<(Mutex<()>, Condvar)>,
}

impl ThreadPool {
    pub fn new(size: usize) -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<(Span, Box<dyn FnOnce() + Send + 'static>)>(0);
        let active_count = Arc::new(AtomicUsize::new(0));
        let completion_pair = Arc::new((Mutex::new(()), Condvar::new()));

        for _ in 0..size {
            let rx = rx.clone();
            let active_count = active_count.clone();
            let completion_pair = completion_pair.clone();

            std::thread::spawn(move || {
                while let Ok((span, job)) = rx.recv() {
                    let _entered = span.enter();
                    let count = active_count.fetch_add(1, Ordering::SeqCst) + 1;
                    tracing::debug!("pool count increased to: {}", count);

                    job();

                    let count = active_count.fetch_sub(1, Ordering::SeqCst) - 1;
                    tracing::debug!("pool count decreased to: {}", count);

                    if count == 0 {
                        let (lock, cvar) = &*completion_pair;
                        let guard = lock.lock().unwrap();
                        cvar.notify_all();
                        drop(guard);
                    }
                }
            });
        }

        ThreadPool {
            tx,
            active_count,
            completion_pair,
        }
    }

    pub fn execute<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let span = Span::current();
        self.tx.send((span, Box::new(f))).unwrap();
    }

    pub fn wait_for_completion(&self) {
        let (lock, cvar) = &*self.completion_pair;
        let mut guard = lock.lock().unwrap();
        while self.active_count.load(Ordering::SeqCst) > 0 {
            guard = cvar.wait(guard).unwrap();
        }
    }
}
