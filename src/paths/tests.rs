use std::path::Path;

use super::expand_template;

#[test]
fn substituted_values_are_not_reinterpreted_as_placeholders() {
    assert_eq!(
        expand_template(
            "{spec_id}|{spec_path}|{plan_path}",
            "001{spec_path}",
            Path::new("specs/{plan_path}.md"),
            Some(Path::new("plans/{spec_id}.md")),
        ),
        "001{spec_path}|specs/{plan_path}.md|plans/{spec_id}.md"
    );
}
