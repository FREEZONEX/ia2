use ironplc_bridge::check;

#[test]
fn error_diagnostics_keep_empty_collections_required_by_the_editor() {
    for source in [
        "PROGRAM main VAR counter : INT; END_VAR counter := missing_signal + 1; END_PROGRAM",
        "PROGRAM main VAR broken END_VAR END_PROGRAM",
    ] {
        let diagnostics = check(source);
        assert!(
            !diagnostics.is_empty(),
            "invalid ST must produce diagnostics"
        );
        for diagnostic in diagnostics {
            let json = serde_json::to_value(diagnostic).unwrap();
            assert!(
                json["context"].is_array(),
                "context must always be an array: {json}"
            );
            assert!(
                json["related"].is_array(),
                "related must always be an array: {json}"
            );
        }
    }
}
