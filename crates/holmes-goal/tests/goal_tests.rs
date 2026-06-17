use holmes_goal::GoalManager;

#[test]
fn test_goal_lifecycle() {
    let mut manager = GoalManager::new();
    assert!(!manager.is_active());

    manager.set("complete the pentest and generate a report or stop after 30 turns");
    assert!(manager.is_active());

    manager.clear("user requested");
    assert!(!manager.is_active());
}
