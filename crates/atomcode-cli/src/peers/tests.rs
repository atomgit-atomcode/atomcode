use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fake {
    session: Mutex<String>,
    received: Mutex<Vec<(String, String)>>,
    notices: AtomicUsize,
}
impl Fake {
    fn new(session: &str) -> Arc<Self> {
        Arc::new(Self {
            session: Mutex::new(session.into()),
            received: Mutex::new(Vec::new()),
            notices: AtomicUsize::new(0),
        })
    }
}
#[async_trait]
impl Target for Fake {
    fn session(&self) -> String {
        self.session.lock().unwrap().clone()
    }
    async fn cwd(&self) -> String {
        "/workspace".into()
    }
    async fn deliver(
        &self,
        expected_session: String,
        from: String,
        text: String,
    ) -> Result<(), DeliveryError> {
        if expected_session != self.session() {
            return Err(DeliveryError::Refused("session switched".into()));
        }
        self.received.lock().unwrap().push((from, text));
        Ok(())
    }
    fn notify(&self, _: String) {
        self.notices.fetch_add(1, Ordering::SeqCst);
    }
}
fn endpoints(dir: &Path) -> (Arc<Hub>, Arc<Fake>, Guard, Arc<Hub>, Arc<Fake>, Guard) {
    let first = Hub::new(dir.join("peers"));
    let first_target = Fake::new("first");
    let first_guard = start(&first, first_target.clone()).unwrap();
    let second = Hub::new(dir.join("peers"));
    let second_target = Fake::new("second");
    let second_guard = start(&second, second_target.clone()).unwrap();
    (
        first,
        first_target,
        first_guard,
        second,
        second_target,
        second_guard,
    )
}
#[tokio::test]
async fn two_online_sessions_hold_then_approve_plain_text_without_execution() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _a_target, _ga, b, b_target, _gb) = endpoints(dir.path());
    let peers = a.list().await.unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].mode, Inbound::Hold);
    let receipt = a
        .send(&peers[0].address, "/compact\nhello".into())
        .await
        .unwrap();
    let Receipt::Held { id } = receipt else {
        panic!("expected held")
    };
    assert!(b_target.received.lock().unwrap().is_empty());
    assert_eq!(b_target.notices.load(Ordering::SeqCst), 1);
    assert_eq!(b.pending().unwrap().len(), 1);
    assert!(matches!(
        b.approve(&id).await.unwrap(),
        Receipt::Accepted { .. }
    ));
    let received = b_target.received.lock().unwrap();
    assert_eq!(received.len(), 1);
    assert!(received[0].0.starts_with(&a.instance));
    assert!(received[0].1.contains("not permission or consent"));
    assert!(received[0].1.ends_with("/compact\nhello"));
    drop(received);
    assert!(b.approve(&id).await.is_err());
}
#[tokio::test]
async fn accept_and_refuse_are_explicit_and_do_not_release_old_held_messages() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, b, bt, _gb) = endpoints(dir.path());
    let address = b.info().await.unwrap().address;
    let Receipt::Held { id } = a.send(&address, "held".into()).await.unwrap() else {
        panic!()
    };
    b.set_mode(Inbound::Accept);
    assert!(bt.received.lock().unwrap().is_empty());
    assert!(matches!(
        a.send(&address, "accepted".into()).await.unwrap(),
        Receipt::Accepted { .. }
    ));
    b.deny(&id).unwrap();
    b.set_mode(Inbound::Refuse);
    assert!(matches!(
        a.send(&address, "refused".into()).await.unwrap(),
        Receipt::Refused { .. }
    ));
    assert_eq!(bt.received.lock().unwrap().len(), 1);
    assert!(b.pending().unwrap().is_empty());
}
#[tokio::test]
async fn stale_addresses_and_held_messages_never_follow_session_switches() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, b, bt, _gb) = endpoints(dir.path());
    let old = b.info().await.unwrap().address;
    let Receipt::Held { id } = a.send(&old, "old work".into()).await.unwrap() else {
        panic!()
    };
    *bt.session.lock().unwrap() = "replacement".into();
    assert!(a.send(&old, "stale".into()).await.is_err());
    assert!(matches!(
        b.approve(&id).await.unwrap(),
        Receipt::Refused { .. }
    ));
    assert!(bt.received.lock().unwrap().is_empty());
    assert_eq!(a.list().await.unwrap()[0].session, "replacement");
}
#[tokio::test]
async fn sender_signature_blocks_forged_or_modified_messages_and_duplicates_do_not_wake_twice() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, b, bt, _gb) = endpoints(dir.path());
    b.set_mode(Inbound::Accept);
    let payload = Message {
        id: uuid::Uuid::new_v4().to_string(),
        from: a.info().await.unwrap().address,
        to: b.info().await.unwrap().address,
        text: "signed".into(),
    };
    let mut message = SignedMessage {
        mac: sign(&a.key, &serde_json::to_vec(&payload).unwrap()),
        payload,
    };
    assert!(matches!(
        b.receive(message.clone()).await.unwrap(),
        Receipt::Accepted { .. }
    ));
    assert!(matches!(
        b.receive(message.clone()).await.unwrap(),
        Receipt::Accepted { .. }
    ));
    assert_eq!(bt.received.lock().unwrap().len(), 1);
    message.payload.text = "forged".into();
    assert!(b.receive(message).await.is_err());
    assert_eq!(bt.received.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn shutdown_removes_endpoint_and_discovery_does_not_list_offline_instances() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, b, _bt, gb) = endpoints(dir.path());
    let address = b.info().await.unwrap().address;
    let path = gb.path.clone();
    drop(gb);
    assert!(!path.exists());
    assert!(a.list().await.unwrap().is_empty());
    assert!(a.send(&address, "offline".into()).await.is_err());
}
#[tokio::test]
async fn inbound_rate_and_frame_sizes_are_bounded() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, b, _bt, _gb) = endpoints(dir.path());
    let address = b.info().await.unwrap().address;
    assert!(a.send(&address, "x".repeat(TEXT_LIMIT + 1)).await.is_err());
    assert!(a.send(&address, " \n".into()).await.is_err());
    for _ in 0..RATE_LIMIT {
        assert!(matches!(
            a.send(&address, "bounded".into()).await.unwrap(),
            Receipt::Held { .. }
        ));
    }
    assert!(a.send(&address, "over rate".into()).await.is_err());
    assert_eq!(b.pending().unwrap().len(), RATE_LIMIT);
    b.state.lock().unwrap().arrivals.clear();
    for _ in RATE_LIMIT..QUEUE_LIMIT {
        assert!(matches!(
            a.send(&address, "queue".into()).await.unwrap(),
            Receipt::Held { .. }
        ));
    }
    assert!(matches!(
        a.send(&address, "over queue".into()).await.unwrap(),
        Receipt::Refused { .. }
    ));
}
#[test]
fn private_directories_reject_symlinks_and_public_permissions() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let private = dir.path().join("private");
    private_directory(&private).unwrap();
    let link = dir.path().join("link");
    symlink(&private, &link).unwrap();
    assert!(private_directory(&link).is_err());
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(private_directory(&private).is_err());
    for invalid in [
        "../evil",
        "",
        "abcd:other",
        "0123456789abcdef0123456789abcdef:../x",
    ] {
        assert!(address_instance(invalid).is_err());
    }
}
#[test]
fn sender_mac_matches_rfc_4231_and_is_constant_work_for_equal_length_inputs() {
    assert_eq!(
        sign(&[0x0b; 20], b"Hi There"),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    assert!(equal_mac(&"a".repeat(64), &"a".repeat(64)));
    assert!(!equal_mac(&"a".repeat(64), &"b".repeat(64)));
    assert!(!equal_mac("", ""));
}

struct EchoFactory;
struct EchoProvider;
impl atomcode_coding::CodingProviderFactory for EchoFactory {
    fn build(
        &self,
        _: &atomcode_coding::CodingAgentConfig,
        _: Option<&str>,
    ) -> Result<Arc<dyn atomcode_kernel::provider::LlmProvider>, atomcode_coding::ProviderBuildError>
    {
        Ok(Arc::new(EchoProvider))
    }
}
#[async_trait]
impl atomcode_kernel::provider::LlmProvider for EchoProvider {
    fn model_name(&self) -> &str {
        "peer-test"
    }
    async fn chat_stream(
        &self,
        messages: &[atomcode_kernel::message::Message],
        _: &[atomcode_kernel::tool::ToolDef],
        _: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        use atomcode_kernel::stream::StreamEvent;
        let first = match messages.iter().rev().find(|message| !message.synthetic) {
            Some(message) if message.role == atomcode_kernel::message::Role::User && message.text.starts_with("relay ") => {
                StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                    id: "send-peer".into(), name: "send_message".into(),
                    arguments: serde_json::json!({"target":message.text.trim_start_matches("relay "), "message":"from a model tool call"}).to_string(),
                })
            },
            _ => StreamEvent::TextDelta("acknowledged".into()),
        };
        Ok(Box::pin(futures::stream::iter([
            first,
            StreamEvent::Done { truncated: false },
        ])))
    }
}
async fn hosted(
    root: &Path,
    name: &str,
) -> (atomcode_host_api::HostConnection, Arc<FrontEnd>, Arc<Hub>) {
    hosted_with_deny(root, name, false).await
}
async fn hosted_with_deny(
    root: &Path,
    name: &str,
    deny: bool,
) -> (atomcode_host_api::HostConnection, Arc<FrontEnd>, Arc<Hub>) {
    let project = root.join(name);
    std::fs::create_dir_all(&project).unwrap();
    let frontend = FrontEnd::new();
    let dirs = atomcode_capabilities::ProductDirs::new(root.join("home"), ".peer-test");
    let mut config = atomcode_coding::CodingAgentConfig::new(
        "key",
        "https://example.test/v1",
        "peer-test",
        &project,
        dirs,
    );
    config.interactive = true;
    if deny {
        let (rules, invalid) =
            atomcode_capabilities::tools::PermissionRules::parse(&[], &["send_message".into()]);
        assert!(invalid.is_empty());
        config.permission_rules = Arc::new(rules);
    }

    let mut prepare = atomcode_coding::PrepareOptions {
        session: atomcode_coding::SessionMode::Fresh,
        front_end: Some(frontend.clone()),
        memory: false,
        web: false,
        mcp: false,
        review: false,
        skill_dirs: Some(Vec::new()),
        subagents: atomcode_coding::SubagentPolicy::Disabled,
        ..Default::default()
    };
    install(&mut prepare, root.join("peers"), &frontend).unwrap();
    let runtime = atomcode_coding::CodingRuntime::start(atomcode_coding::CodingRuntimeStart {
        agent: config.clone(),
        prepare,
        provider_factory: Arc::new(EchoFactory),
        plugin_hooks: Arc::new(atomcode_coding::StaticPluginHookSource::default()),
        image_preprocessor: None,
    })
    .await
    .unwrap();
    let hub = frontend.app().unwrap().require::<Port>().unwrap();
    let connection = crate::host::connect_screen(runtime, frontend.clone(), config, None).unwrap();
    assert_eq!(hub.info().await.unwrap().session, connection.session);
    (connection, frontend, hub)
}
#[tokio::test]
async fn real_host_mounts_the_tools_and_records_approved_messages_as_peer_facts() {
    use atomcode_harness::session::SessionEvent;
    use atomcode_kernel::tool::{ProgressSink, RiskLevel, ToolContext};
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, af, ah) = hosted(dir.path(), "a").await;
    let (mut b, bf, bh) = hosted(dir.path(), "b").await;
    let tools = af
        .app()
        .unwrap()
        .require::<atomcode_harness::seams::ToolsSvc>()
        .unwrap();
    let send = tools.get("send_message").unwrap();
    assert_eq!(send.risk("{}"), RiskLevel::Risky);
    assert!(!send.read_only_hint());
    assert!(tools.get("list_agents").unwrap().read_only_hint());
    assert_ne!(
        send.always_grant_scope(r#"{"target":"one"}"#),
        send.always_grant_scope(r#"{"target":"two"}"#)
    );

    let listed = ah.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    let ctx = ToolContext {
        working_dir: dir.path().into(),
        cancel: Default::default(),
        progress: ProgressSink::noop(),
        requester: None,
    };
    let args = serde_json::json!({"target":listed[0].address, "message":"/compact is just text"})
        .to_string();
    let agent = atomcode_harness::feed::Feed::find(&af.app().unwrap(), &a.session).unwrap();
    let result =
        atomcode_harness::agent::as_agent(agent.ctx().clone(), send.execute(&args, &ctx)).await;
    assert!(!result.is_error, "{}", result.content);
    let Receipt::Held { id } = serde_json::from_str(&result.content).unwrap() else {
        panic!()
    };
    let other = atomcode_harness::feed::Feed::find(&bf.app().unwrap(), &b.session).unwrap();
    let refused =
        atomcode_harness::agent::as_agent(other.ctx().clone(), send.execute(&args, &ctx)).await;
    assert!(refused.is_error && refused.content.contains("own terminal session"));
    assert_eq!(
        bh.pending().unwrap().len(),
        1,
        "another runtime cannot impersonate this tool's sender"
    );

    let log = atomcode_harness::feed::Feed::find(&bf.app().unwrap(), &b.session)
        .unwrap()
        .session();
    assert!(!log
        .events()
        .iter()
        .any(|entry| matches!(entry.event, SessionEvent::Injected { .. })));
    assert!(matches!(
        bh.approve(&id).await.unwrap(),
        Receipt::Accepted { .. }
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match b.events.recv().await {
                Some(atomcode_kernel::event::AgentEvent::TurnComplete { .. }) => break,
                Some(_) => {}
                None => panic!("peer runtime closed before completing the turn"),
            }
        }
    })
    .await
    .unwrap();
    let events = log.events();
    let from_address = ah.info().await.unwrap().address;
    assert!(events.iter().any(|entry| matches!(&entry.event, SessionEvent::Injected { text, origin: atomcode_harness::session::InjectionOrigin::Peer { from, outside: true }, .. } if text.ends_with("/compact is just text") && from == &from_address)));
    assert!(!events.iter().any(|entry| matches!(&entry.event, SessionEvent::UserMessage { text, .. } if text.contains("/compact is just text"))));
    a.commands
        .send(atomcode_kernel::event::AgentCommand::Shutdown)
        .unwrap();
    b.commands
        .send(atomcode_kernel::event::AgentCommand::Shutdown)
        .unwrap();
}

#[tokio::test]
async fn remote_protocol_cannot_approve_messages_or_change_inbound_policy() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, b, bt, _gb) = endpoints(dir.path());
    let Receipt::Held { id } = a
        .send(
            &b.info().await.unwrap().address,
            "await local consent".into(),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    for operation in ["approve", "mode", "execute"] {
        let mut stream = UnixStream::connect(b.directory.join(format!("{}.sock", b.instance)))
            .await
            .unwrap();
        let bytes =
            format!("{{\"operation\":\"{operation}\",\"id\":\"{id}\",\"value\":\"accept\"}}\n");
        stream.write_all(bytes.as_bytes()).await.unwrap();
        let response: Response =
            serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
        assert!(matches!(response, Response::Error(_)));
    }
    assert_eq!(b.info().await.unwrap().mode, Inbound::Hold);
    assert_eq!(b.pending().unwrap().len(), 1);
    assert!(bt.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn refusing_a_held_message_updates_its_cached_receipt() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, b, _bt, _gb) = endpoints(dir.path());
    let payload = Message {
        id: uuid::Uuid::new_v4().to_string(),
        from: a.info().await.unwrap().address,
        to: b.info().await.unwrap().address,
        text: "held".into(),
    };
    let message = SignedMessage {
        mac: sign(&a.key, &serde_json::to_vec(&payload).unwrap()),
        payload,
    };
    assert!(matches!(
        b.receive(message.clone()).await.unwrap(),
        Receipt::Held { .. }
    ));
    b.set_mode(Inbound::Refuse);
    assert!(b.pending().unwrap().is_empty());
    assert!(matches!(
        b.receive(message).await.unwrap(),
        Receipt::Refused { .. }
    ));
}

#[tokio::test]
async fn wire_frames_reject_control_escapes_and_oversize_input() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, b, _bt, _gb) = endpoints(dir.path());
    let address = b.info().await.unwrap().address;
    for text in [
        "escape\x1b[31m",
        "nul\0",
        "carriage\rreturn",
        "control\u{009b}",
    ] {
        assert!(a.send(&address, text.into()).await.is_err());
    }
    assert!(matches!(
        a.send(&address, "line\r\nnext".into()).await.unwrap(),
        Receipt::Held { .. }
    ));
    assert_eq!(b.pending().unwrap()[0].message.text, "line\nnext");
    let mut stream = UnixStream::connect(b.directory.join(format!("{}.sock", b.instance)))
        .await
        .unwrap();
    stream
        .write_all(&vec![b'x'; FRAME_LIMIT as usize + 1])
        .await
        .unwrap();
    let response: Response =
        serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
    assert!(matches!(response, Response::Error(_)));
    assert_eq!(b.pending().unwrap().len(), 1);
}

#[tokio::test]
async fn incompatible_protocol_versions_fail_explicitly() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (_a, _at, _ga, b, _bt, _gb) = endpoints(dir.path());
    let mut stream = UnixStream::connect(b.directory.join(format!("{}.sock", b.instance)))
        .await
        .unwrap();
    stream
        .write_all(b"{\"version\":2,\"request\":{\"operation\":\"info\"}}\n")
        .await
        .unwrap();
    let response: Response =
        serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
    assert!(matches!(response, Response::Error(reason) if reason.contains("protocol version")));
}

// This fixture is run only by the process-interoperability test below.
#[tokio::test]
#[ignore = "subprocess fixture"]
async fn child_peer_fixture() {
    let root =
        PathBuf::from(std::env::var_os("ATOMCODE_PEER_FIXTURE_DIR").expect("fixture directory"));
    let hub = Hub::new(root.join("peers"));
    let target = Fake::new("child-process");
    let _guard = start(&hub, target.clone()).unwrap();
    hub.set_mode(Inbound::Accept);
    std::fs::write(
        root.join("ready.json"),
        serde_json::to_vec(&hub.info().await.unwrap()).unwrap(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let received = target.received.lock().unwrap().clone();
            if !received.is_empty() {
                std::fs::write(
                    root.join("received.json"),
                    serde_json::to_vec(&received).unwrap(),
                )
                .unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

// Uses real clocks because the other OS process and Unix socket/file readiness
// do not advance with this process's virtual Tokio clock.
#[tokio::test]
async fn independent_processes_discover_and_exchange_authenticated_text() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let sender = Hub::new(dir.path().join("peers"));
    let target = Fake::new("parent-process");
    let _guard = start(&sender, target.clone()).unwrap();
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "peers::local::tests::child_peer_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("ATOMCODE_PEER_FIXTURE_DIR", dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let info: PeerInfo = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = std::fs::read(dir.path().join("ready.json")) {
                if let Ok(info) = serde_json::from_slice(&bytes) {
                    break info;
                }
            }
            if let Some(status) = child.0.try_wait().unwrap() {
                panic!("child exited before discovery: {status}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(sender.list().await.unwrap(), vec![info.clone()]);
    assert!(matches!(
        sender
            .send(&info.address, "cross-process report".into())
            .await
            .unwrap(),
        Receipt::Accepted { .. }
    ));
    let received: Vec<(String, String)> = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = std::fs::read(dir.path().join("received.json")) {
                if let Ok(received) = serde_json::from_slice(&bytes) {
                    break received;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(received.len(), 1);
    assert!(received[0].1.ends_with("cross-process report"));
    assert_eq!(received[0].0, sender.info().await.unwrap().address);
}

#[tokio::test]
async fn a_model_send_uses_the_sender_permission_prompt_and_then_the_receiver_hold() {
    use atomcode_kernel::event::{AgentCommand, AgentEvent};
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (mut a, _af, _ah) = hosted(dir.path(), "a").await;
    let (b, _bf, bh) = hosted(dir.path(), "b").await;
    let address = bh.info().await.unwrap().address;
    a.control
        .call(atomcode_host_api::HostCommand::SetMode {
            session: a.session.clone(),
            mode: atomcode_host_api::Mode::Ask,
        })
        .await
        .unwrap();
    a.commands
        .send(AgentCommand::SendMessage {
            text: format!("relay {address}"),
            images: Vec::new(),
        })
        .unwrap();
    let mut permission_prompt = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match a.events.recv().await {
                Some(AgentEvent::Request { id, kind, .. }) => {
                    assert_eq!(kind, atomcode_capabilities::tools::APPROVAL_KIND);
                    permission_prompt = true;
                    // This is the local user's explicit response, not peer data.
                    let value = serde_json::to_value(
                        atomcode_capabilities::tools::ApprovalResponse::allow(),
                    )
                    .unwrap();
                    a.commands
                        .send(AgentCommand::Respond { id, value })
                        .unwrap();
                }
                Some(AgentEvent::TurnComplete { .. }) => break,
                Some(_) => {}
                None => panic!("sender stopped"),
            }
        }
    })
    .await
    .unwrap();
    assert!(
        permission_prompt,
        "a model send must use the sender's existing approval seam; facts: {:?}",
        atomcode_harness::feed::Feed::find(&_af.app().unwrap(), &a.session)
            .unwrap()
            .session()
            .events()
    );
    let pending = bh.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].message.text, "from a model tool call");
    a.commands.send(AgentCommand::Shutdown).unwrap();
    b.commands.send(AgentCommand::Shutdown).unwrap();
}

#[tokio::test]
async fn explicit_sender_deny_rules_prevent_network_delivery() {
    use atomcode_kernel::event::{AgentCommand, AgentEvent};
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (mut a, af, _ah) = hosted_with_deny(dir.path(), "a", true).await;
    let (b, _bf, bh) = hosted(dir.path(), "b").await;
    a.commands
        .send(AgentCommand::SendMessage {
            text: format!("relay {}", bh.info().await.unwrap().address),
            images: Vec::new(),
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match a.events.recv().await {
                Some(AgentEvent::Request { .. }) => {
                    panic!("deny rules should refuse without asking")
                }
                Some(AgentEvent::TurnComplete { .. }) => break,
                Some(_) => {}
                None => panic!("sender stopped"),
            }
        }
    })
    .await
    .unwrap();
    assert!(bh.pending().unwrap().is_empty());
    let events = atomcode_harness::feed::Feed::find(&af.app().unwrap(), &a.session)
        .unwrap()
        .session()
        .events();
    assert!(events.iter().any(|entry| matches!(&entry.event, atomcode_harness::session::SessionEvent::ToolResultLogged { content, is_error:true, .. } if content.contains("deny"))));
    a.commands.send(AgentCommand::Shutdown).unwrap();
    b.commands.send(AgentCommand::Shutdown).unwrap();
}

#[tokio::test]
async fn an_uncertain_runtime_receipt_is_never_reported_as_refusal_or_retried() {
    struct Ambiguous {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl Target for Ambiguous {
        fn session(&self) -> String {
            "uncertain-target".into()
        }
        async fn cwd(&self) -> String {
            "/workspace".into()
        }
        async fn deliver(&self, _: String, _: String, _: String) -> Result<(), DeliveryError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(DeliveryError::Uncertain(
                "the runtime stopped before confirming".into(),
            ))
        }
        fn notify(&self, _: String) {}
    }
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, _unused, _ut, _ug) = endpoints(dir.path());
    let b = Hub::new(dir.path().join("peers"));
    let target = Arc::new(Ambiguous {
        calls: AtomicUsize::new(0),
    });
    let _gb = start(&b, target.clone()).unwrap();
    b.set_mode(Inbound::Accept);
    let payload = Message {
        id: uuid::Uuid::new_v4().to_string(),
        from: a.info().await.unwrap().address,
        to: b.info().await.unwrap().address,
        text: "may already have arrived".into(),
    };
    let message = SignedMessage {
        mac: sign(&a.key, &serde_json::to_vec(&payload).unwrap()),
        payload,
    };
    assert!(matches!(
        b.receive(message.clone()).await.unwrap(),
        Receipt::Uncertain { .. }
    ));
    assert!(matches!(
        b.receive(message).await.unwrap(),
        Receipt::Uncertain { .. }
    ));
    assert_eq!(target.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_replacement_session_has_room_without_opening_the_pending_list() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, b, bt, _gb) = endpoints(dir.path());
    let address = b.info().await.unwrap().address;
    let mut old_ids = Vec::new();
    for _ in 0..QUEUE_LIMIT {
        // Isolate queue capacity from the independent per-minute admission limit.
        b.state.lock().unwrap().arrivals.clear();
        let Receipt::Held { id } = a.send(&address, "old work".into()).await.unwrap() else {
            panic!("expected held")
        };
        old_ids.push(id);
    }
    *bt.session.lock().unwrap() = "replacement".into();
    b.state.lock().unwrap().arrivals.clear();
    let address = b.info().await.unwrap().address;
    let Receipt::Held { id } = a.send(&address, "new work".into()).await.unwrap() else {
        panic!("old held messages must not fill the replacement queue")
    };
    let pending = b.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].message.id, id);
    let state = b.state.lock().unwrap();
    for old_id in old_ids {
        let receipt = &state.seen.iter().find(|entry| entry.0 == old_id).unwrap().2;
        assert!(
            matches!(receipt, Receipt::Refused { reason } if reason.contains("session changed"))
        );
    }
}

#[tokio::test]
async fn dead_socket_files_do_not_count_towards_online_discovery_capacity() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, b, _bt, _gb) = endpoints(dir.path());
    let mut stale_paths = Vec::new();
    for _ in 0..160 {
        let path = a
            .directory
            .join(format!("{}.sock", uuid::Uuid::new_v4().simple()));
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        stale_paths.push(path);
    }
    let peers = a.list().await.unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].address, b.info().await.unwrap().address);
    // Discovery has no filesystem deletion race with a restarting peer.
    assert!(stale_paths.iter().all(|path| path.exists()));
}

#[tokio::test]
async fn opening_pending_after_a_session_switch_updates_discarded_receipts() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (a, _at, _ga, b, bt, _gb) = endpoints(dir.path());
    let Receipt::Held { id } = a
        .send(&b.info().await.unwrap().address, "old work".into())
        .await
        .unwrap()
    else {
        panic!("expected held")
    };
    *bt.session.lock().unwrap() = "replacement".into();
    assert!(b.pending().unwrap().is_empty());
    assert!(
        matches!(&b.state.lock().unwrap().seen.iter().find(|entry| entry.0 == id).unwrap().2,
        Receipt::Refused { reason } if reason.contains("session changed"))
    );
}
