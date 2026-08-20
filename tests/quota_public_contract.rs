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
    assert_eq!(plan_row("Free"), ["Free", "Free", "60", "--", "4", "200"]);
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
    // The plan gate used to be section headers inside the server instructions,
    // which enumerated every tool. That enumeration moved onto the tools
    // themselves, so assert the gate where a client now reads it: once in the
    // instructions, and again on each tool that is actually gated.
    let flat = server.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("additionally need an academic or business plan"),
        "server instructions must still state the paid-plan gate"
    );
    for paid in [
        "merge_collections",
        "batch_move_entries",
        "generate_citation_keys",
        "expand_journal",
        "normalize_bibliography",
    ] {
        let desc = tool_description(&flat, paid)
            .unwrap_or_else(|| panic!("no description found for {paid}"));
        assert!(
            desc.contains("academic or business plan"),
            "{paid} is plan-gated but its own description does not say so"
        );
    }
    // Sharing is open to any signed-in account with collections, so those tools
    // must not claim a paid plan.
    for free in [
        "share_collection",
        "unshare_collection",
        "view_shared",
        "usage",
        "list_styles",
        "batch_resolve",
        "enhanced_search",
        "orcid_search",
        "orcid_profile",
        "ingest_orcid",
        "update_entry_metadata",
        "merge_entries",
    ] {
        let desc = tool_description(&flat, free)
            .unwrap_or_else(|| panic!("no description found for {free}"));
        assert!(
            !desc.contains("academic or business plan"),
            "{free} is not plan-gated but its description claims a plan"
        );
    }
    assert!(readme.contains("free account (60 lookups/day)"));
    assert!(cli.contains("anonymous mode (20 lookups/day)"));
    assert!(setup.contains("anonymous mode: 20 lookups/day"));
    assert!(constants.contains("IP daily limit ~20"));
    assert!(batch_limits.contains("~20/day anonymous"));

    for required in [
        "setup --connect",
        "platform credential store",
        "--store-command",
        "--retrieve-command",
        "--credential-file",
        "standard input",
        "setup --key",
        "OOKCITE_API_KEY_COMMAND",
        "OOKCITE_API_KEY_FILE",
    ] {
        assert!(
            readme.contains(required),
            "README.md is missing secure connection guidance: {required}"
        );
    }
    assert!(!readme.contains("pass show"));
}

/// Pull one tool's `description = "..."` out of a whitespace-flattened
/// `server.rs`, so the plan-gate assertions read the same text a client does.
fn tool_description(flat: &str, tool: &str) -> Option<String> {
    let anchor = format!("name = \"{tool}\", description = \"");
    let start = flat.find(&anchor)? + anchor.len();
    let rest = &flat[start..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(esc) = chars.next() {
                    out.push(esc);
                }
            }
            '"' => return Some(out),
            _ => out.push(c),
        }
    }
    None
}
