//! Cooperative cancellation of blocking RPC work.
//!
//! Subset of upstream `reth-tasks::cancel` (reth#27462) that [`is_cancelled`] needs.

use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

/// Sets the `cancelled` flag when any clone is dropped.
#[derive(Default, Clone, Debug)]
pub struct CancelOnDrop(Arc<AtomicBool>);

impl CancelOnDrop {
    /// Returns true if the job was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Runs `f` with this as the cancellation state of the current thread, see [`is_cancelled`].
    ///
    /// The previous state of the thread is restored once `f` returns or unwinds. Leaving the
    /// scope does not cancel.
    pub fn scope<R>(&self, f: impl FnOnce() -> R) -> R {
        struct Restore(Option<Arc<AtomicBool>>);

        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT.set(self.0.take());
            }
        }

        let _restore = Restore(CURRENT.replace(Some(self.0.clone())));
        f()
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Returns `true` if the work running on the current thread was cancelled.
///
/// Reads the state of the innermost [`CancelOnDrop::scope`] on this thread. Long running work
/// should check this between units of work, such as transactions. Always `false` outside a scope.
pub fn is_cancelled() -> bool {
    CURRENT.with_borrow(|state| state.as_ref().is_some_and(|state| state.load(Ordering::Relaxed)))
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scope_is_cancelled() {
        assert!(!is_cancelled());

        let outer = CancelOnDrop::default();
        let inner = CancelOnDrop::default();
        outer.scope(|| {
            assert!(!is_cancelled());
            inner.scope(|| {
                drop(inner.clone());
                assert!(is_cancelled());
            });
            // the outer state is restored
            assert!(!is_cancelled());
        });
        assert!(!outer.is_cancelled());
    }

    #[test]
    fn test_scope_restores_on_panic() {
        let cancel = CancelOnDrop::default();
        drop(cancel.clone());
        let res = std::panic::catch_unwind(|| cancel.scope(|| panic!("scope panicked")));
        assert!(res.is_err());
        assert!(!is_cancelled());
    }
}
