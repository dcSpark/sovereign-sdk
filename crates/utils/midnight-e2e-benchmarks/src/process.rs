/// Simple guard that terminates the child process when dropped.
pub struct ChildGuard(pub std::process::Child);

impl ChildGuard {
    pub fn new(child: std::process::Child) -> Self {
        Self(child)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}
