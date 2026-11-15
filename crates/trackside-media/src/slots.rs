//! A counting semaphore whose waiters are served by priority, then in arrival order.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::oneshot;

use crate::{MediaError, Priority};

#[derive(Debug)]
struct State {
    free: usize,
    waiters: [VecDeque<oneshot::Sender<SlotPermit>>; 3],
}

fn rank(priority: Priority) -> usize {
    match priority {
        Priority::Interactive => 0,
        Priority::Prefetch => 1,
        Priority::Download => 2,
    }
}

/// Transcode slots shared by every job.
#[derive(Clone, Debug)]
pub(crate) struct Slots {
    state: Arc<Mutex<State>>,
}

/// A held slot; dropping it hands the slot to the highest-priority waiter.
#[derive(Debug)]
pub(crate) struct SlotPermit {
    state: Option<Arc<Mutex<State>>>,
}

impl Drop for SlotPermit {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            release(&state);
        }
    }
}

fn release(state: &Arc<Mutex<State>>) {
    let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
    for queue in &mut guard.waiters {
        while let Some(waiter) = queue.pop_front() {
            let permit = SlotPermit {
                state: Some(state.clone()),
            };
            match waiter.send(permit) {
                Ok(()) => return,
                // The waiter gave up; defuse the permit so it does not release again while
                // the lock is held.
                Err(mut permit) => permit.state = None,
            }
        }
    }
    guard.free += 1;
}

impl Slots {
    pub(crate) fn new(count: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                free: count.max(1),
                waiters: Default::default(),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn free(&self) -> usize {
        self.lock().free
    }

    /// Waiters still interested in a slot.
    pub(crate) fn waiting(&self) -> usize {
        self.lock()
            .waiters
            .iter()
            .flatten()
            .filter(|w| !w.is_closed())
            .count()
    }

    pub(crate) fn try_acquire(&self) -> Option<SlotPermit> {
        let mut guard = self.lock();
        (guard.free > 0).then(|| {
            guard.free -= 1;
            SlotPermit {
                state: Some(self.state.clone()),
            }
        })
    }

    /// Waits for a slot. Cancel-safe: dropping the future gives up the place in the queue.
    pub(crate) async fn acquire(&self, priority: Priority) -> Result<SlotPermit, MediaError> {
        let rx = {
            let mut guard = self.lock();
            if guard.free > 0 {
                guard.free -= 1;
                return Ok(SlotPermit {
                    state: Some(self.state.clone()),
                });
            }
            let (tx, rx) = oneshot::channel();
            let queue = &mut guard.waiters[rank(priority)];
            queue.retain(|w| !w.is_closed());
            queue.push_back(tx);
            rx
        };
        rx.await
            .map_err(|_| MediaError::Process("transcode slots closed".into()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    #[tokio::test]
    async fn priority_order_is_respected() -> Result<(), MediaError> {
        let slots = Slots::new(1);
        let held = slots.try_acquire();
        assert!(held.is_some());
        let order = Arc::new(StdMutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for (name, priority) in [
            ("download", Priority::Download),
            ("prefetch", Priority::Prefetch),
            ("interactive", Priority::Interactive),
            ("download2", Priority::Download),
        ] {
            let task_slots = slots.clone();
            let order = order.clone();
            tasks.push(tokio::spawn(async move {
                let permit = task_slots.acquire(priority).await;
                order
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(name);
                drop(permit);
            }));
            while slots.waiting() < tasks.len() {
                tokio::task::yield_now().await;
            }
        }
        drop(held);
        for task in tasks {
            task.await.map_err(|e| MediaError::Process(e.to_string()))?;
        }
        let order = order.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(order, ["interactive", "prefetch", "download", "download2"]);
        assert_eq!(slots.free(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn abandoned_waiter_does_not_leak_a_slot() {
        let slots = Slots::new(1);
        let held = slots.try_acquire();
        let waiting = slots.acquire(Priority::Interactive);
        let timed_out = tokio::time::timeout(std::time::Duration::from_millis(1), waiting).await;
        assert!(timed_out.is_err());
        drop(held);
        assert_eq!(slots.free(), 1);
    }
}
