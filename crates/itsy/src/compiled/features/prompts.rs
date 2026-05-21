//! Templates used for
//! feature-level prompts (multi-file edits, verify-and-fix loop).

pub const VERIFY_AND_FIX: &str = include_str!("../../assets/prompts/verify_and_fix.txt");
pub const MULTI_FILE_EDIT: &str = include_str!("../../assets/prompts/multi_file_edit.txt");
pub const SEMANTIC_MERGE: &str = include_str!("../../assets/prompts/semantic_merge.txt");
pub const ERROR_DIAGNOSIS: &str = include_str!("../../assets/prompts/error_diagnosis.txt");
