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
