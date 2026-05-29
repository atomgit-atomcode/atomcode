use super::registry::SubAgentRegistry;
use super::types::SubAgentDefinition;

#[test]
fn test_registry_register_and_find() {
    let reg = SubAgentRegistry::new();
    let def = SubAgentDefinition {
        name: "test-agent".into(),
        description: "test agent".into(),
        ..Default::default()
    };
    assert!(reg.register(def).is_ok());
    assert!(reg.find("test-agent").is_some());
    assert!(reg.find("nonexistent").is_none());
}

#[test]
fn test_registry_duplicate_rejected() {
    let reg = SubAgentRegistry::new();
    let def = SubAgentDefinition {
        name: "dup".into(),
        ..Default::default()
    };
    assert!(reg.register(def.clone()).is_ok());
    assert!(reg.register(def).is_err());
}

#[test]
fn test_registry_list() {
    let reg = SubAgentRegistry::new();
    let def1 = SubAgentDefinition {
        name: "a".into(),
        ..Default::default()
    };
    let def2 = SubAgentDefinition {
        name: "b".into(),
        ..Default::default()
    };
    reg.register(def1).unwrap();
    reg.register(def2).unwrap();
    assert_eq!(reg.list().len(), 2);
}
