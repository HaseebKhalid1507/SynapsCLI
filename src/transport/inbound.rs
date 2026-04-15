#[derive(Debug, Clone)]
pub enum Inbound {
    Message { content: String },
    Steer { content: String },
    Cancel,
    Command { name: String, args: String },
    SyncRequest,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbound_variants() {
        let _message = Inbound::Message { content: "hello".to_string() };
        let _steer = Inbound::Steer { content: "please fix".to_string() };
        let _cancel = Inbound::Cancel;
        let _command = Inbound::Command { name: "save".to_string(), args: "file.txt".to_string() };
        let _sync = Inbound::SyncRequest;
    }
}