use holmes_cli::commands::CommandRegistry;

#[test]
fn test_all_commands_registered() {
    let registry = CommandRegistry::default();
    // Should have at least 6 categories
    assert!(registry.list_by_category().len() >= 6);

    // All essential commands should be resolvable
    assert_eq!(registry.resolve("help"), Some("help"));
    assert_eq!(registry.resolve("quit"), Some("quit"));
    assert_eq!(registry.resolve("exit"), Some("quit"));
    assert_eq!(registry.resolve("q"), Some("quit"));
    assert_eq!(registry.resolve("new"), Some("new"));
    assert_eq!(registry.resolve("reset"), Some("new"));
    assert_eq!(registry.resolve("resume"), Some("resume"));
    assert_eq!(registry.resolve("sessions"), Some("sessions"));
    assert_eq!(registry.resolve("history"), Some("sessions"));
    assert_eq!(registry.resolve("branch"), Some("branch"));
    assert_eq!(registry.resolve("fork"), Some("branch"));
    assert_eq!(registry.resolve("status"), Some("status"));
    assert_eq!(registry.resolve("goal"), Some("goal"));
    assert_eq!(registry.resolve("model"), Some("model"));
    assert_eq!(registry.resolve("config"), Some("config"));
    assert_eq!(registry.resolve("tools"), Some("tools"));
    assert_eq!(registry.resolve("workflows"), Some("workflows"));
    assert_eq!(registry.resolve("dashboard"), Some("dashboard"));
    assert_eq!(registry.resolve("usage"), Some("usage"));
    assert_eq!(registry.resolve("save"), Some("save"));
    assert_eq!(registry.resolve("export"), Some("save"));
}

#[test]
fn test_alias_chains_dont_loop() {
    let registry = CommandRegistry::default();
    // Verify that resolving an alias gives a canonical name that can be resolved again
    let fork = registry.resolve("fork").unwrap();
    let resolved_again = registry.resolve(fork);
    assert_eq!(resolved_again, Some(fork)); // canonical name resolves to itself
}

#[test]
fn test_help_displays_all_categories() {
    let registry = CommandRegistry::default();
    let cats = registry.list_by_category();
    for (cat, cmds) in &cats {
        assert!(!cat.is_empty());
        assert!(!cmds.is_empty());
        for cmd in cmds {
            assert!(!cmd.name.is_empty());
        }
    }
}

#[test]
fn test_unique_canonical_names() {
    let registry = CommandRegistry::default();
    let mut names: Vec<&str> = registry.list_by_category()
        .iter()
        .flat_map(|(_, cmds)| cmds.iter().map(|c| c.name))
        .collect();
    let len_before = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), len_before, "Duplicate canonical command names found");
}
