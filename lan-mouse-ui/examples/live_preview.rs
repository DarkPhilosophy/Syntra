fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (reader, writer) = lan_mouse_ipc::connect_with_timeout(std::time::Duration::from_secs(5))?;
    lan_mouse_ui::app::run_with_ipc(reader, writer)
}
