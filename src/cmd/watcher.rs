pub async fn run(command: String, args: Vec<String>) {
    crate::watcher::run(command, args).await;
}
