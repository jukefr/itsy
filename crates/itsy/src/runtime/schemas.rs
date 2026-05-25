//! Reserved for compiled MarrowScript
//! schema helpers used by other compiled modules. Currently a stub mirroring
//! the original module's no-op surface.

#[derive(Debug, Default, Clone)]
pub struct CompiledSchemas;

impl CompiledSchemas {
    pub const fn new() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_default_are_equivalent() {
        let _a = CompiledSchemas::new();
        let _b: CompiledSchemas = CompiledSchemas::default();
        // Both compile; both are zero-sized — pin the no-op contract.
        assert_eq!(std::mem::size_of::<CompiledSchemas>(), 0);
    }
}
