pub struct MessageBytes {
    pub uid: u32,
    pub bytes: Vec<u8>,
}

impl MessageBytes {
    pub fn new(uid: u32, bytes: Vec<u8>) -> Self {
        Self { uid, bytes }
    }
}
