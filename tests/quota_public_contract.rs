#[test]
fn public_quota_claims_match_the_runtime_contract() {
    let readme = include_str!("../README.md");
    let cli = include_str!("../src/cli.rs");
    let setup = include_str!("../src/setup.rs");
    let constants = include_str!("../src/constants.rs");
    let batch_limits = include_str!("../src/batch_limits.rs");

    for (path, contents) in [
        ("README.md", readme),
        ("src/cli.rs", cli),
        ("src/setup.rs", setup),
        ("src/constants.rs", constants),
        ("src/batch_limits.rs", batch_limits),
    ] {
        for stale in [
            "10 lookups/day",
            "30 lookups/day",
            "~10/day anonymous",
            "IP daily limit ~10",
        ] {
            assert!(
                !contents.contains(stale),
                "{path} contains stale OokCite quota claim: {stale}"
            );
        }
    }

    assert!(readme.contains("| Anonymous | Free    | 20"));
    assert!(readme.contains("free account (60 lookups/day)"));
    assert!(cli.contains("anonymous mode (20 lookups/day)"));
    assert!(setup.contains("anonymous mode: 20 lookups/day"));
    assert!(constants.contains("IP daily limit ~20"));
    assert!(batch_limits.contains("~20/day anonymous"));
}
