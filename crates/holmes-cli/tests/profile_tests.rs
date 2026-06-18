use std::fs;

#[test]
fn test_active_profile_read_write() {
    let dir = std::env::temp_dir().join(format!("holmes_test_profile_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let active_file = dir.join("active_profile");
    fs::write(&active_file, "pentest\n").unwrap();
    let name = fs::read_to_string(&active_file).unwrap();
    assert_eq!(name.trim(), "pentest");
    fs::remove_dir_all(&dir).ok();
}
