//! No-op backend for non-Windows builds (tests / CI on Linux). Records the last
//! diff so unit tests can assert on it.

use crate::input::backends::InputBackend;
use crate::input::state::InputDiff;

#[derive(Default)]
pub struct NoopBackend {
    pub last: Option<InputDiff>,
    pub released: bool,
}

impl InputBackend for NoopBackend {
    fn name(&self) -> &str {
        "noop"
    }
    fn apply_diff(&mut self, diff: &InputDiff) {
        self.last = Some(diff.clone());
    }
    fn release_all(&mut self) {
        self.released = true;
    }
}
