pub const OBJECT_TYPE_CANDIDATES: [&str; 7] = [
    "snapshot",
    "tree",
    "file",
    "chunk",
    "directory_root",
    "tree_shard",
    "file_shard",
];

pub fn candidate_object_types(hint: Option<&str>) -> Vec<&'static str> {
    let mut candidates = Vec::with_capacity(OBJECT_TYPE_CANDIDATES.len());
    if let Some(hint) = hint
        && OBJECT_TYPE_CANDIDATES.contains(&hint)
    {
        candidates.push(match hint {
            "snapshot" => "snapshot",
            "tree" => "tree",
            "file" => "file",
            "chunk" => "chunk",
            "directory_root" => "directory_root",
            "tree_shard" => "tree_shard",
            "file_shard" => "file_shard",
            _ => unreachable!(),
        });
    }
    for object_type in OBJECT_TYPE_CANDIDATES {
        if !candidates.contains(&object_type) {
            candidates.push(object_type);
        }
    }
    candidates
}

pub fn infer_object_type_from_hint<F>(hint: Option<&str>, mut matches_type: F) -> &'static str
where
    F: FnMut(&str) -> bool,
{
    for object_type in candidate_object_types(hint) {
        if matches_type(object_type) {
            return object_type;
        }
    }
    "chunk"
}

#[cfg(test)]
mod tests {
    use super::{candidate_object_types, infer_object_type_from_hint};

    #[test]
    fn candidate_object_types_prioritize_known_hint_without_duplicates() {
        assert_eq!(
            candidate_object_types(Some("tree")),
            vec![
                "tree",
                "snapshot",
                "file",
                "chunk",
                "directory_root",
                "tree_shard",
                "file_shard",
            ]
        );
    }

    #[test]
    fn infer_object_type_from_hint_checks_hinted_type_first() {
        let mut seen = Vec::new();

        let inferred = infer_object_type_from_hint(Some("file"), |object_type| {
            seen.push(object_type.to_string());
            object_type == "file"
        });

        assert_eq!(inferred, "file");
        assert_eq!(seen, vec!["file"]);
    }

    #[test]
    fn infer_object_type_from_hint_falls_back_after_wrong_hint() {
        let mut seen = Vec::new();

        let inferred = infer_object_type_from_hint(Some("snapshot"), |object_type| {
            seen.push(object_type.to_string());
            object_type == "file"
        });

        assert_eq!(inferred, "file");
        assert_eq!(seen, vec!["snapshot", "tree", "file"]);
    }

    #[test]
    fn candidate_object_types_include_file_shard() {
        assert_eq!(
            candidate_object_types(Some("file_shard")),
            vec![
                "file_shard",
                "snapshot",
                "tree",
                "file",
                "chunk",
                "directory_root",
                "tree_shard",
            ]
        );
    }
}
