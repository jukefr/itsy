//! Smart file listing for projects:
//! keyword-scored, ignore-aware.

use std::path::Path;

use ignore::WalkBuilder;

pub fn format_smart_listing(root: &Path, hint: &str, max: usize) -> String {
    let mut entries: Vec<(String, u32)> = Vec::new();
    let walker = WalkBuilder::new(root)
        .standard_filters(true)
        .max_depth(Some(4))
        .build();
    let hint_words: Vec<String> = hint.to_lowercase().split_whitespace().map(String::from).collect();
    for dent in walker.flatten() {
        if dent.depth() == 0 {
            continue;
        }
        let path = dent.path();
        let rel = path.strip_prefix(root).unwrap_or(path).display().to_string();
        let lower = rel.to_lowercase();
        let score = hint_words.iter().filter(|w| !w.is_empty() && lower.contains(w.as_str())).count() as u32;
        entries.push((rel, score));
    }
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    entries.truncate(max);
    if entries.is_empty() {
        return "(empty directory)".into();
    }
    entries.into_iter().map(|(p, _)| format!("  {p}")).collect::<Vec<_>>().join("\n")
}
