use slint::{Color, ComponentHandle};

fn snapshot(app: &syntra_ui::app::AppWindow, name: &str) {
    match app.window().take_snapshot() {
        Ok(image) => {
            let path = format!("/tmp/syntra-{name}.ppm");
            let mut data = format!("P6\n{} {}\n255\n", image.width(), image.height()).into_bytes();
            let mut min = [u8::MAX; 4];
            let mut max = [u8::MIN; 4];
            let mut opaque = 0usize;
            let mut transparent = 0usize;
            for pixel in image.as_slice() {
                let channels = [pixel.r, pixel.g, pixel.b, pixel.a];
                for (index, value) in channels.into_iter().enumerate() {
                    min[index] = min[index].min(value);
                    max[index] = max[index].max(value);
                }
                opaque += usize::from(pixel.a == u8::MAX);
                transparent += usize::from(pixel.a == 0);
                // PPM has no alpha channel: composite over white instead of
                // turning transparent pixels into the black diagnostic block.
                let alpha = u16::from(pixel.a);
                data.extend_from_slice(&[
                    ((u16::from(pixel.r) * alpha + 255 * (255 - alpha)) / 255) as u8,
                    ((u16::from(pixel.g) * alpha + 255 * (255 - alpha)) / 255) as u8,
                    ((u16::from(pixel.b) * alpha + 255 * (255 - alpha)) / 255) as u8,
                ]);
            }
            std::fs::write(&path, data).unwrap();
            println!(
                "SNAPSHOT {name} {}x{} rgba-min={min:?} rgba-max={max:?} opaque={opaque} transparent={transparent} path={path}",
                image.width(),
                image.height(),
            );
        }
        Err(error) => eprintln!("SNAPSHOT ERROR {name}: {error}"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = syntra_ui::app::create_app()?;
    app.global::<syntra_ui::app::Translations>()
        .on_translate(|key, _| match key.as_str() {
            "welcome-title" | "app-title" => "Syntra".into(),
            "welcome-motto" => "Move freely. Stay connected.".into(),
            _ => key,
        });

    let weak = app.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_millis(300),
        move || {
            let app = weak.upgrade().unwrap();
            snapshot(&app, "welcome");
            app.set_launch_ready(true);

            let weak = app.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
                let app = weak.upgrade().unwrap();
                snapshot(&app, "main-default");

                let theme = app.global::<syntra_ui::app::Theme>();
                theme.set_background(Color::from_rgb_u8(7, 18, 23));
                theme.set_surface(Color::from_rgb_u8(15, 30, 36));
                theme.set_elevated_surface(Color::from_rgb_u8(24, 43, 51));
                theme.set_header(Color::from_rgb_u8(10, 24, 29));
                theme.set_sidebar(Color::from_rgb_u8(12, 27, 33));
                theme.set_border(Color::from_rgb_u8(45, 72, 82));
                theme.set_text(Color::from_rgb_u8(237, 248, 250));
                theme.set_muted_text(Color::from_rgb_u8(165, 189, 195));
                theme.set_selection(Color::from_rgb_u8(33, 67, 77));
                theme.set_hover_surface(Color::from_rgb_u8(27, 53, 62));
                theme.set_pressed_surface(Color::from_rgb_u8(38, 70, 81));
                theme.set_accent(Color::from_rgb_u8(128, 199, 255));
                theme.set_base_mode("black".into());
                theme.set_palette("ice".into());

                let weak = app.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(100), move || {
                    let app = weak.upgrade().unwrap();
                    snapshot(&app, "main-dark-ice");
                    println!(
                        "STATE launch-visible={} mode={} palette={}",
                        app.get_launch_visible(),
                        app.global::<syntra_ui::app::Theme>().get_base_mode(),
                        app.global::<syntra_ui::app::Theme>().get_palette(),
                    );
                    slint::quit_event_loop().unwrap();
                });
            });
        },
    );
    app.run()?;
    Ok(())
}
