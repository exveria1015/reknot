use super::RouteError;
use std::cell::Cell;
use std::rc::Rc;

/// Shared conservative lifetime load budget. KeepLoaded never refunds a slot,
/// even for a rejected/abandoned native candidate. Share across router recreation.
#[derive(Clone, Debug)]
pub struct GenerationBudget(Rc<Budget>);
#[derive(Debug)]
struct Budget {
    limit: usize,
    used: Cell<usize>,
}
impl GenerationBudget {
    pub fn new(limit: usize) -> Result<Self, RouteError> {
        if limit == 0 {
            return Err(RouteError::InvalidLimits);
        }
        Ok(Self(Rc::new(Budget {
            limit,
            used: Cell::new(0),
        })))
    }
    pub fn used(&self) -> usize {
        self.0.used.get()
    }
    pub fn limit(&self) -> usize {
        self.0.limit
    }
    pub(super) fn reserve(&self) -> Result<(), RouteError> {
        if self.used() >= self.limit() {
            return Err(RouteError::GenerationLimit {
                limit: self.limit(),
            });
        }
        self.0.used.set(self.used() + 1);
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct RouterOptions {
    pub generations: GenerationBudget,
    pub max_checkpoint_bytes: usize,
}
impl Default for RouterOptions {
    fn default() -> Self {
        Self {
            generations: GenerationBudget::new(32).unwrap(),
            max_checkpoint_bytes: 1024 * 1024,
        }
    }
}
