#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatEntry {
    pub id: u64,
    pub role: Role,
    pub content: String,
}
