use atomcode_daemon::SkillInfo;

#[test]
fn skill_api_keeps_the_invocation_name_and_adds_an_optional_title() {
    let mut skill = SkillInfo {
        name: "packager".into(),
        display_name: None,
        description: "Build packages".into(),
    };
    assert_eq!(
        serde_json::to_value(&skill).unwrap(),
        serde_json::json!({
            "name": "packager", "description": "Build packages"
        })
    );
    skill.display_name = Some("玲珑打包技能".into());
    let value = serde_json::to_value(&skill).unwrap();
    assert_eq!(value["name"], "packager");
    assert_eq!(value["display_name"], "玲珑打包技能");
}
