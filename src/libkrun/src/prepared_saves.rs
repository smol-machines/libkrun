//! One retained checkpoint generation, including its active output worker.
//! A missing entry means no worker owns the generation, not that an artifact
//! is durable. Artifact completion is still reported by the output operation.

use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Preparing,
    Ready,
    Finishing,
}

enum Entry<T> {
    Preparing,
    Ready(T),
    Finishing,
}

pub(crate) struct PreparedSaves<T> {
    entry: Mutex<Option<(String, Entry<T>)>>,
}

impl<T> PreparedSaves<T> {
    pub(crate) const fn new() -> Self {
        Self {
            entry: Mutex::new(None),
        }
    }

    pub(crate) fn reserve(&self, id: &str) -> Option<Preparation<'_, T>> {
        let mut entry = self.entry.lock().unwrap();
        if entry.is_some() {
            return None;
        }
        *entry = Some((id.to_owned(), Entry::Preparing));
        Some(Preparation {
            saves: self,
            armed: true,
        })
    }

    pub(crate) fn status(&self, id: &str) -> Option<Status> {
        let entry = self.entry.lock().unwrap();
        let (current, state) = entry.as_ref()?;
        if current != id {
            return None;
        }
        Some(match state {
            Entry::Preparing => Status::Preparing,
            Entry::Ready(_) => Status::Ready,
            Entry::Finishing => Status::Finishing,
        })
    }

    /// The callback must finish using the generation before returning. Keep
    /// the slot occupied during output, failures, and generation destruction.
    pub(crate) fn finish<R>(&self, id: &str, output: impl FnOnce(T) -> R) -> Option<R> {
        let value = {
            let mut entry = self.entry.lock().unwrap();
            let (current, state) = entry.as_mut()?;
            if current != id || !matches!(state, Entry::Ready(_)) {
                return None;
            }
            let Entry::Ready(value) = std::mem::replace(state, Entry::Finishing) else {
                unreachable!()
            };
            value
        };
        // Declared after taking the value, but the value is moved into output:
        // its argument drops before this guard on both return and unwind.
        let _finishing = Completion { saves: self };
        Some(output(value))
    }

    pub(crate) fn cancel(&self, id: &str) -> bool {
        self.finish(id, drop).is_some()
    }
}

pub(crate) struct Preparation<'a, T> {
    saves: &'a PreparedSaves<T>,
    armed: bool,
}

impl<T> Preparation<'_, T> {
    pub(crate) fn publish(mut self, value: T) {
        let mut entry = self.saves.entry.lock().unwrap();
        let (_, state) = entry.as_mut().expect("preparation owns the occupied slot");
        *state = Entry::Ready(value);
        self.armed = false;
    }
}

impl<T> Drop for Preparation<'_, T> {
    fn drop(&mut self) {
        if self.armed {
            *self.saves.entry.lock().unwrap() = None;
        }
    }
}

struct Completion<'a, T> {
    saves: &'a PreparedSaves<T>,
}

impl<T> Drop for Completion<'_, T> {
    fn drop(&mut self) {
        *self.saves.entry.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::time::Duration;

    #[test]
    fn output_remains_registered_until_the_worker_finishes() {
        let saves = PreparedSaves::new();
        saves.reserve("first").unwrap().publish(7);
        let (started, running) = mpsc::channel();
        let (release, released) = mpsc::channel();
        std::thread::scope(|scope| {
            let saves = &saves;
            let worker = scope.spawn(move || {
                saves.finish("first", |value| {
                    started.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(5)).unwrap();
                    value + 1
                })
            });
            running.recv_timeout(Duration::from_secs(5)).unwrap();
            let status = saves.status("first");
            let second_refused = saves.reserve("second").is_none();
            let cancellation_refused = !saves.cancel("first");
            let duplicate_refused = saves.finish("first", |_| ()).is_none();
            release.send(()).unwrap();
            assert_eq!(worker.join().unwrap(), Some(8));
            assert_eq!(status, Some(Status::Finishing));
            assert!(second_refused);
            assert!(cancellation_refused);
            assert!(duplicate_refused);
        });
        assert_eq!(saves.status("first"), None);
        assert!(saves.reserve("second").is_some());
    }

    #[test]
    fn failed_preparation_and_wrong_identifiers_do_not_lose_a_generation() {
        let saves = PreparedSaves::new();
        let reservation = saves.reserve("first").unwrap();
        assert_eq!(saves.status("first"), Some(Status::Preparing));
        assert!(!saves.cancel("first"));
        drop(reservation);
        assert_eq!(saves.status("first"), None);
        saves.reserve("next").unwrap().publish(9);
        assert!(!saves.cancel("wrong"));
        assert!(saves.finish("wrong", |_| ()).is_none());
        assert_eq!(saves.status("next"), Some(Status::Ready));
        assert_eq!(saves.finish("next", Err::<(), _>), Some(Err(9)));
        assert_eq!(saves.status("next"), None);
    }

    #[test]
    fn cancellation_drops_the_generation_before_releasing_the_slot() {
        struct Held(Arc<PreparedSaves<Self>>, Arc<AtomicBool>);
        impl Drop for Held {
            fn drop(&mut self) {
                assert_eq!(self.0.status("first"), Some(Status::Finishing));
                assert!(self.0.reserve("next").is_none());
                self.1.store(true, Ordering::SeqCst);
            }
        }
        let saves = Arc::new(PreparedSaves::new());
        let dropped = Arc::new(AtomicBool::new(false));
        saves
            .reserve("first")
            .unwrap()
            .publish(Held(saves.clone(), dropped.clone()));
        assert!(saves.cancel("first"));
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(saves.status("first"), None);
    }
}
