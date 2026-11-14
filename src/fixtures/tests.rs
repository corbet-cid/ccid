use super::*;

#[test]
fn parse_takes_exactly_the_declared_names() {
    let declared = vec!["A".to_owned(), "B".to_owned()];
    assert_eq!(parse("f", &declared, "A=1\nB=2=3\n").unwrap()[1].1, "2=3");
    for bad in [
        "A=1\n",
        "A=1\nB=2\nC=3\n",
        "A=1\nB=\n",
        "A=1\nA=2\nB=3\n",
        "A=1\nB\n",
    ] {
        assert!(parse("f", &declared, bad).is_err(), "{bad:?}");
    }
}
