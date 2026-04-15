use synaps_cli::{Runtime, Session};
use synaps_cli::transport::{ConversationDriver, StdioTransport};
use synaps_cli::transport::driver::DriverConfig;

#[tokio::main]
async fn main() -> synaps_cli::Result<()> {
    let _log_guard = synaps_cli::logging::init_logging();

    let config = synaps_cli::config::load_config();
    let mut runtime = Runtime::new().await?;
    runtime.apply_config(&config);

    let system_prompt = synaps_cli::config::resolve_system_prompt(None);
    runtime.set_system_prompt(system_prompt);

    let session = Session::new(runtime.model(), runtime.thinking_level(), runtime.system_prompt());

    println!("💬 Terminal Chat (Transport Edition)");
    println!("Model: {} | Thinking: {}", runtime.model(), runtime.thinking_level());
    println!("Type your message. Press Ctrl+C to exit.\n");

    let driver_config = DriverConfig {
        agent_name: None,
        auto_save: true,
        event_buffer_size: 50,
    };
    let mut driver = ConversationDriver::new(runtime, session, driver_config);

    let transport = StdioTransport::new();
    let sync = driver.sync_state();
    driver.bus().connect(transport, sync);

    driver.run().await?;

    Ok(())
}
