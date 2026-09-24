use thiserror::Error;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("parse error: {0}")]
    Parse(String),

    #[error("analyze error: {0}")]
    Analyze(String),

    #[error("codegen error: {0}")]
    Codegen(String),
}

/// A name a graphical transpiler would declare twice once IEC 61131-3's
/// case-insensitive name rules apply, with the diagram element to
/// highlight (`L` is that language's location type). The transpile paths
/// refuse it as a `BridgeError::Parse`; the check path also keeps the
/// location so the editor can point at the rung or block.
#[derive(Debug, Clone)]
pub struct NameClash<L> {
    pub message: String,
    pub location: L,
}

impl<L> From<NameClash<L>> for BridgeError {
    fn from(clash: NameClash<L>) -> Self {
        BridgeError::Parse(clash.message)
    }
}
