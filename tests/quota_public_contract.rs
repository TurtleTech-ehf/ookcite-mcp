#[test]
fn public_quota_claims_match_the_runtime_contract() {
    let readme = include_str!("../README.md");
    let npm_readme = include_str!("../npm/README.md");
    let cli = include_str!("../src/cli.rs");
    let setup = include_str!("../src/setup.rs");
    let constants = include_str!("../src/constants.rs");
    let batch_limits = include_str!("../src/batch_limits.rs");
    let server = include_str!("../src/server.rs");

    for (path, contents) in [
        ("README.md", readme),
        ("npm/README.md", npm_readme),
        ("src/cli.rs", cli),
        ("src/setup.rs", setup),
        ("src/constants.rs", constants),
        ("src/batch_limits.rs", batch_limits),
    ] {
        for stale in [
            "10 lookups/day",
            "30 lookups/day",
            "30/day with a free account",
            "~10/day anonymous",
            "IP daily limit ~10",
            "$4/month",
            "$12/month",
        ] {
            assert!(
                !contents.contains(stale),
                "{path} contains stale OokCite quota claim: {stale}"
            );
        }
    }

    assert_eq!(
        npm_readme, readme,
        "the npm package README must mirror the public root contract"
    );

    let plan_row = |tier: &str| {
        readme
            .lines()
            .find(|line| line.starts_with(&format!("| {tier}")))
            .unwrap_or_else(|| panic!("README.md is missing the {tier} plan row"))
            .split('|')
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        plan_row("Anonymous"),
        ["Anonymous", "Free", "20", "--", "0", "--"]
    );
    assert_eq!(
        plan_row("Free"),
        ["Free", "Free", "60", "--", "4", "200"]
    );
    assert_eq!(
        plan_row("Academic"),
        ["Academic", "EUR 4/mo", "20,000", "10,000", "10", "1,000"]
    );
    assert_eq!(
        plan_row("Business"),
        ["Business", "EUR 10/mo", "20,000", "40,000", "20", "4,000"]
    );
    assert!(readme.contains("Sharing is available to signed-in accounts"));
    assert!(readme.contains("Free accounts can import and batch-add"));
    assert!(readme.contains("Merge and batch-move require an Academic or Business plan"));
    assert!(!readme.contains("Sharing & Bulk Operations (requires academic/business plan)"));
    assert!(!readme.contains("Batch operations require an academic or business plan"));
    assert!(!server.contains("SHARING (academic/business plan)"));
    assert!(!server.contains("BULK OPERATIONS (academic/business plan)"));
    assert!(!server.contains("Anyone with the link can view it. Requires academic/business plan."));
    assert!(server.contains("SHARING (signed-in accounts with collections)"));
    assert!(server.contains("PAID COLLECTION OPERATIONS (academic/business plan)"));
    assert!(readme.contains("free account (60 lookups/day)"));
    assert!(cli.contains("anonymous mode (20 lookups/day)"));
    assert!(setup.contains("anonymous mode: 20 lookups/day"));
    assert!(constants.contains("IP daily limit ~20"));
    assert!(batch_limits.contains("~20/day anonymous"));
}
