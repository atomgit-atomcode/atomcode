use atomcode_config::{config::SearchToolConfig, Config};
#[test]
fn search_settings_are_backwards_compatible_and_round_trip() {
    let old: Config = toml::from_str("").unwrap();
    assert!(old.tools.search.respect_gitignore);
    assert!(!old.tools.search.include_hidden);
    assert!(!old.tools.search.follow_symlinks);
    assert!(!toml::to_string(&old).unwrap().contains("[tools.search]"));
    let cfg:Config=toml::from_str("[tools.search]\nrespect_gitignore=false\ninclude_hidden=true\nfollow_symlinks=true\ntimeout_secs=12\nmax_entries=5000\n").unwrap();
    let again: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(again.tools.search, cfg.tools.search);
}
#[test]
fn project_search_overrides_only_named_fields_and_rejects_invalid_files() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("search.toml");
    let global = SearchToolConfig {
        include_hidden: true,
        timeout_secs: 12,
        ..Default::default()
    };
    assert_eq!(global.with_project_file(&file).unwrap(), global);
    std::fs::write(&file, "respect_gitignore=false\nfollow_symlinks=true").unwrap();
    let project = global.with_project_file(&file).unwrap();
    assert!(!project.respect_gitignore);
    assert!(project.follow_symlinks);
    assert!(project.include_hidden);
    assert_eq!(project.timeout_secs, 12);
    for bad in ["include_hidden='yes'", "unknown=true", "include_hidden = ["] {
        std::fs::write(&file, bad).unwrap();
        assert!(global.with_project_file(&file).is_err());
    }
}
