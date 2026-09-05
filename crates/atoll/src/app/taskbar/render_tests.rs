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
