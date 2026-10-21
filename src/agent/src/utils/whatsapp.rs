use whatsapp_rust::Client;

pub async fn list_groups_ids(client: &Client) -> Result<Vec<String>, String> {
    let groups = client.groups().get_participating().await;
    match groups {
        Ok(groups) => {
            let mut groups_ids = Vec::new();
            for (id, group) in groups {
                let group_str = format!("{} - {}", id.clone(), group.subject.clone());
                groups_ids.push(group_str.clone());
                log::info!("{}", group_str);
            }
            Ok(groups_ids)
        }
        Err(e) => Err(e.to_string()),
    }
}
