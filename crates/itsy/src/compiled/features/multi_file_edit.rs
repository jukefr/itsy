//! Helper for multi-file
//! edit planning.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditPlan {
    pub edits: Vec<PlannedEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedEdit {
    pub path: String,
    pub summary: String,
}
