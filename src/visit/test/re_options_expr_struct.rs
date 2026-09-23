//! Tests that the `--re` and `--exclude-re` options filter the field-deletion
//! mutants of struct literal expressions, like every other mutant.

use indoc::indoc;
use test_log::test;

use crate::Options;
use crate::visit::mutate_source_str;

const STRUCT_LITERAL_WITH_DEFAULT: &str = indoc! {r#"
    #[derive(Default)]
    struct Settings {
        enabled: bool,
        count: i32,
    }

    fn make() -> Settings {
        Settings {
            enabled: true,
            count: 1,
            ..Default::default()
        }
    }
"#};

fn mutant_names(args: &[&str]) -> Vec<String> {
    let options = Options::from_arg_strs(args);
    mutate_source_str(STRUCT_LITERAL_WITH_DEFAULT, &options)
        .unwrap()
        .iter()
        .map(|m| m.name(false))
        .collect()
}

#[test]
fn exclude_re_option_filters_struct_field_deletions() {
    let names = mutant_names(&["mutants", "--exclude-re", "delete field enabled"]);
    assert!(
        !names.iter().any(|n| n.contains("delete field enabled")),
        "delete `enabled` field mutant should be excluded: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("delete field count")),
        "delete `count` field mutant should remain: {names:?}"
    );
}

#[test]
fn re_option_filters_struct_field_deletions() {
    let names = mutant_names(&["mutants", "--re", "replace make"]);
    assert!(
        names.iter().any(|n| n.contains("replace make")),
        "the matching mutant should remain: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("delete field")),
        "field-deletion mutants don't match --re, so they should be filtered: {names:?}"
    );
}
