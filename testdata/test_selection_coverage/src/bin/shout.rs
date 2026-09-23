use cargo_mutants_testdata_test_selection_coverage::shout;

fn main() {
    let words: Vec<String> = std::env::args().skip(1).collect();
    println!("{}", shout(&words.join(" ")));
}
