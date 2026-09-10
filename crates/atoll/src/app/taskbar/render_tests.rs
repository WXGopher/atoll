//! Exercise readout updates and sizing without creating desktop windows.

use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::{PlatformError, Rgb8Pixel};

use super::{Along, Chip, TaskbarView};
use atoll_core::protocol::HookSource;
use atoll_core::state::AgentTasks;

struct TestPlatform(Rc<MinimalSoftwareWindow>);

impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn draw(window: &MinimalSoftwareWindow) -> Option<Vec<Rgb8Pixel>> {
    let mut pixels = None;
    window.draw_if_needed(|renderer| {
        let size = window.size();
        let mut buffer = vec![Rgb8Pixel::default(); (size.width * size.height) as usize];
        renderer.render(&mut buffer, size.width as usize);
        pixels = Some(buffer);
    });
    pixels
}

#[test]
fn idle_readout_rescales_its_pixels_and_repairs_size_without_changing_chips() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(Rc::clone(&window)))).unwrap();
    let bar = TaskbarView::new(super::TaskbarBar::new().unwrap());
    let chips = [Chip {
        agent: Some(HookSource::Codex),
        value: "72%".into(),
        tier: "good",
        tasks: AgentTasks {
            done: 1,
            ..Default::default()
        },
    }];
    bar.show();
    bar.set_chips(&chips, Along::Vertical);
    let mut dot_at_100 = 0;
    for scale in [1.0_f32, 1.25, 1.5, 2.0, 1.0, 1.5] {
        bar.sync_scale(scale);
        assert_eq!(window.scale_factor(), scale);
        assert_eq!(
            bar.physical_size(),
            ((45.0 * scale).round() as i32, (39.0 * scale).round() as i32)
        );
        let pixels = draw(&window).expect("DPI changes repaint idle content");
        if let Some(dir) = std::env::var_os("ATOLL_RENDER_DIR") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let size = window.size();
            let mut bytes = format!("P6\n{} {}\n255\n", size.width, size.height).into_bytes();
            bytes.extend(pixels.iter().flat_map(|pixel| [pixel.r, pixel.g, pixel.b]));
            std::fs::write(dir.join(format!("readout-{scale}.ppm")), bytes).unwrap();
        }
        let blue = pixels
            .iter()
            .filter(|pixel| i32::from(pixel.b) - i32::from(pixel.r) > 60)
            .count();
        if scale == 1.0 {
            dot_at_100 = blue;
            assert!(dot_at_100 > 0);
        } else {
            // Enlarging only the native window leaves the dot at its old
            // pixel size. Verify the rendered content grows with the DPI too.
            let ratio = blue as f32 / dot_at_100 as f32;
            assert!(
                (ratio - scale * scale).abs() < 0.6,
                "dot area ratio {ratio}"
            );
        }
        bar.set_chips(&chips, Along::Vertical);
        bar.sync_scale(scale);
        bar.breathe(0.25);
        assert!(draw(&window).is_none(), "unchanged DPI stays idle");
    }

    // Windows can resize the child without changing Slint's scale.
    window.set_size(slint::PhysicalSize::new(45, 39));
    let _ = draw(&window);
    bar.sync_scale(1.5);
    assert_eq!(bar.physical_size(), (68, 59));
    assert!(draw(&window).is_some());
}

#[test]
fn readout_updates_colours_and_layout_without_scheduling_idle_frames() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(Rc::clone(&window)))).unwrap();
    let bar = TaskbarView::new(super::TaskbarBar::new().unwrap());
    let mut chips = vec![
        Chip {
            agent: Some(HookSource::Claude),
            value: "23%".into(),
            tier: "warn",
            tasks: AgentTasks::default(),
        },
        Chip {
            agent: Some(HookSource::Codex),
            value: "34%".into(),
            tier: "warn",
            tasks: AgentTasks::default(),
        },
    ];

    bar.show();
    for scale in [1.0, 1.25, 1.5, 2.0] {
        window.dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        for along in [Along::Vertical, Along::Horizontal] {
            for tasks in [
                AgentTasks::default(),
                AgentTasks {
                    done: 2,
                    ..Default::default()
                },
            ] {
                chips[0].tasks = tasks;
                bar.set_chips(&chips, along);
                let clean = draw(&window).expect("changed content requests a frame");
                assert!(clean.iter().any(|pixel| pixel.r != pixel.b));
                let size = window.size();
                assert_eq!((size.width as i32, size.height as i32), bar.physical_size());

                bar.request_redraw();
                assert!(draw(&window).unwrap() == clean, "{scale}, {along:?}");

                // Unchanged data and finished sessions still schedule no frames.
                bar.set_chips(&chips, along);
                bar.breathe(0.25);
                assert!(draw(&window).is_none());
            }
        }
    }

    bar.request_redraw();
    let previous = draw(&window).unwrap();
    // Editing colour thresholds must refresh even if the number stays 23%.
    chips[0].tier = "low";
    bar.set_chips(&chips, Along::Horizontal);
    assert!(draw(&window).expect("a tier change requests a frame") != previous);

    bar.hide();
    let _ = draw(&window);
    bar.request_redraw();
    assert!(
        draw(&window).is_none(),
        "hidden readouts do not request frames"
    );
}
