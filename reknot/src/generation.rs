//! Code leases independent of the optional native loader.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Releasing a lease never closes a library. Only the host's explicit reclaimer
/// may do that, after checking both owners and returned calls.
#[derive(Clone)]
pub(crate) struct CodeLease {
    _code: Arc<dyn Send + Sync>,
    executions: Arc<AtomicUsize>,
    #[cfg(feature = "native")]
    native_id: Option<crate::ArtifactId>,
}
impl CodeLease {
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn new(code: Arc<dyn Send + Sync>, executions: Arc<AtomicUsize>) -> Self {
        Self {
            _code: code,
            executions,
            #[cfg(feature = "native")]
            native_id: None,
        }
    }
    #[cfg(feature = "native")]
    pub(crate) fn with_native_id(mut self, id: crate::ArtifactId) -> Self {
        self.native_id = Some(id);
        self
    }
    #[cfg(feature = "native")]
    pub(crate) fn native_id(&self) -> Option<crate::ArtifactId> {
        self.native_id
    }
    pub(crate) fn enter(&self) -> CallGuard {
        self.executions.fetch_add(1, Ordering::AcqRel);
        CallGuard(self.executions.clone())
    }
}

/// Lives on the host stack until the foreign call has returned. It deliberately
/// survives destruction of a lease from inside a callback.
pub(crate) struct CallGuard(Arc<AtomicUsize>);
impl Drop for CallGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(crate) fn enter(lease: &Option<CodeLease>) -> Option<CallGuard> {
    lease.as_ref().map(CodeLease::enter)
}

pub(crate) struct GenerationOwner {
    pub(crate) code: CodeLease,
    #[cfg(feature = "native")]
    pub(crate) id: crate::ArtifactId,
}
impl GenerationOwner {
    pub(crate) fn prepared(&self) {
        #[cfg(feature = "native")]
        crate::retention::update(self.id, |v| {
            v.state = crate::ImageState::Prepared;
            v.outcome = crate::ImageOutcome::Candidate;
        });
    }
    pub(crate) fn activate(&self) {
        #[cfg(feature = "native")]
        crate::retention::update(self.id, |v| {
            v.state = crate::ImageState::Active;
            v.outcome = crate::ImageOutcome::Adopted;
        });
    }
    pub(crate) fn retire(&self) {
        #[cfg(feature = "native")]
        crate::retention::update(self.id, |v| v.state = crate::ImageState::Retired);
    }

    pub(crate) fn lease(&self) -> CodeLease {
        self.code.clone()
    }
    pub(crate) fn enter(&self) -> CallGuard {
        self.code.enter()
    }
}
impl Drop for GenerationOwner {
    fn drop(&mut self) {
        #[cfg(feature = "native")]
        crate::retention::update(self.id, |v| {
            v.state = crate::ImageState::Retired;
            v.outcome = match v.outcome {
                crate::ImageOutcome::Pending => crate::ImageOutcome::Rejected,
                crate::ImageOutcome::Candidate => crate::ImageOutcome::Abandoned,
                other => other,
            };
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_return_guard_outlives_the_last_code_lease_and_unwinds_cleanly() {
        let calls = Arc::new(AtomicUsize::new(0));
        let code = Arc::new(());
        let lease = CodeLease::new(code.clone(), calls.clone());
        let outer = lease.enter();
        assert_eq!(calls.load(Ordering::Acquire), 1);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _nested = lease.enter();
            assert_eq!(calls.load(Ordering::Acquire), 2);
            panic!("return through the host guard");
        }));
        assert_eq!(calls.load(Ordering::Acquire), 1);
        drop(lease);
        assert_eq!(Arc::strong_count(&code), 1);
        assert_eq!(calls.load(Ordering::Acquire), 1);
        drop(outer);
        assert_eq!(calls.load(Ordering::Acquire), 0);
    }
}
