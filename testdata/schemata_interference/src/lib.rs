//! One tested and one untested function, and tests that are unreliable in ways
//! that have nothing to do with them.

pub fn tested(a: u32) -> u32 {
    a + 1
}

pub fn untested(a: u32) -> u32 {
    a * 3
}

#[cfg(test)]
mod test {
    use std::fs;
    use std::path::Path;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn tested_adds_one() {
        assert_eq!(super::tested(1), 2);
    }

    /// Write a file at a fixed path in the package directory, wait, and read it back,
    /// which fails if another copy of this test writes it meanwhile.
    #[test]
    fn fixed_path_round_trip() {
        let path = Path::new("round_trip.txt");
        let content = std::process::id().to_string();
        fs::write(path, &content).unwrap();
        sleep(Duration::from_millis(500));
        assert_eq!(fs::read_to_string(path).unwrap(), content);
    }

    /// Fail the first time this runs with each schemata mutant id, like a flaky test.
    #[test]
    fn flaky_once_per_mutant() {
        let Ok(id) = std::env::var("CARGO_MUTANTS_SCHEMATA_ID") else {
            return;
        };
        let marker = format!("flaked-{id}");
        if id != "0" && !Path::new(&marker).exists() {
            fs::write(&marker, "").unwrap();
            panic!("simulated flaky failure for mutant {id}");
        }
    }
}
