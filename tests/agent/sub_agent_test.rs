use std::{sync::Arc, time::Duration};

use rust_bot::{
    agent::subagent::SubagentManager,
    bus::{events::InboundMessage, queue::MessageBus},
};

use crate::config::helpers::{create_openrouter_provider, prepare_workspace};

const TASK: &str = "Can you please report me on trending topics in the field of AI? Please provide the links to the articles.";

#[tokio::test]
async fn test_sub_agent() {
    let provider = create_openrouter_provider();
    let bus = Arc::new(MessageBus::new());
    let workspace = prepare_workspace();
    let manager = Arc::new(SubagentManager::new_simple(
        Arc::new(provider),
        workspace,
        bus.clone(),
        4096,
    ));

    Arc::clone(&manager).spawn(TASK, Some("worker-1"), Some("cli"), Some("direct"), None);

    tokio::time::timeout(Duration::from_secs(120), async {
        while bus.inbound_size() == 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("timed out waiting for subagent announce");

    drop(manager);
    let mut bus = match Arc::try_unwrap(bus) {
        Ok(bus) => bus,
        Err(_) => panic!("manager should release bus Arc"),
    };
    let msg: InboundMessage = bus
        .consume_inbound()
        .await
        .expect("announce should publish");

    assert_eq!(msg.channel, "system");
    assert_eq!(msg.sender_id, "subagent");
    println!("msg: {}", msg.content);
    assert!(msg.content.contains("worker-1"));
    assert!(msg.content.contains(TASK));
    assert!(
        msg.content.contains("completed successfully") || msg.content.contains("failed"),
        "unexpected announce status: {}",
        msg.content
    );
}
