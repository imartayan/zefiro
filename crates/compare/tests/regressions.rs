//! Inputs the fuzzer once broke us with: `tests/data/<target>-<name>.bin`,
//! replayed through the body of the fuzz target they came from.

use std::path::Path;

use compare::fuzz;

#[test]
fn fuzz_findings_stay_fixed() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let Some((target, _)) = name.split_once('-') else {
            continue;
        };
        let body: fn(&[u8]) = match (target, path.extension().and_then(|e| e.to_str())) {
            ("decompress", Some("bin")) => fuzz::decompress,
            ("round_trip", Some("bin")) => fuzz::round_trip,
            ("mutate", Some("bin")) => fuzz::mutate,
            ("stream", Some("bin")) => fuzz::stream,
            ("dict", Some("bin")) => fuzz::dict,
            _ => continue,
        };
        body(&std::fs::read(&path).unwrap());
    }
}
