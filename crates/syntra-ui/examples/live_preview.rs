fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (reader, writer) = syntra_api::connect_with_timeout(std::time::Duration::from_secs(5))?;
    syntra_ui::app::run_with_ipc(reader, writer)
}
