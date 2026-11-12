//! The region selection overlay: a small libcosmic application whose only
//! surface is a fullscreen, transparent "layer shell" surface on top of
//! everything (like the screenshot portal), with no regular window.
//!
//! Once the user confirms, the overlay stays on screen as a recording
//! indicator: no handles, no toolbar, and clicks / keys go through to the
//! windows below. It disappears when its process is stopped.

use std::sync::mpsc;

use anyhow::{Result, anyhow};
use cosmic::Element;
use cosmic::app::{self, Task};
use cosmic::cctk::sctk::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
use cosmic::iced::core::keyboard::{self, key::Named};
use cosmic::iced::platform_specific::shell::commands::layer_surface::{
    get_layer_surface, set_input_zone, set_keyboard_interactivity,
};
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::{
    IcedOutput, SctkLayerSurfaceSettings,
};
use cosmic::iced::{Alignment, Color, Event, Length, Rectangle, Subscription, event, window};
use cosmic::widget::{Row, button, container, text};

use crate::region::Region;
use crate::selection::Selection;

/// Shows the overlay. When the user confirms, the selection is sent on
/// `selected` (`None` = the whole screen); the overlay then stays visible
/// until the process is stopped. Returns when the user cancels.
pub fn run(selected: mpsc::Sender<Option<Region>>) -> Result<()> {
    let settings = app::Settings::default()
        .no_main_window(true)
        .exit_on_close(false);
    cosmic::app::run::<Selector>(settings, selected).map_err(|e| anyhow!("overlay failed: {e}"))
}

struct Selector {
    core: app::Core,
    result: mpsc::Sender<Option<Region>>,
    surface: window::Id,
    rect: Option<Rectangle>,
    /// Confirmed: we only show where the recording happens.
    recording: bool,
}

#[derive(Debug, Clone)]
enum Message {
    Changed(Rectangle),
    Confirm,
    Cancel,
}

impl Selector {
    fn confirm(&mut self) -> Task<Message> {
        let region = match self.rect {
            Some(r) if r.width >= 8.0 && r.height >= 8.0 => Some(Region {
                x: r.x.round() as u32,
                y: r.y.round() as u32,
                width: r.width.round() as u32,
                height: r.height.round() as u32,
            }),
            // No selection, or a tiny one (probably a misclick): record the
            // whole screen. (We don't need its size: the capture uses the
            // full frame. libcosmic doesn't report the size of a layer
            // surface through `on_window_resize` anyway.)
            _ => None,
        };
        let _ = self.result.send(region);

        // Show exactly the rounded region that will be recorded (nothing for
        // the whole screen).
        self.rect = region.map(|region| Rectangle {
            x: region.x as f32,
            y: region.y as f32,
            width: region.width as f32,
            height: region.height as f32,
        });
        self.recording = true;
        // An empty input zone lets the mouse go through to the windows below;
        // keyboard: give it back to them too.
        Task::batch([
            set_input_zone(self.surface, Some(Vec::new())),
            set_keyboard_interactivity(self.surface, KeyboardInteractivity::None),
        ])
    }

    fn toolbar(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let label = match self.rect {
            Some(r) => format!("{} × {}", r.width.round(), r.height.round()),
            None => "Drag to select an area, or press Enter for the whole screen".into(),
        };
        let bar = Row::new()
            .push(text::body(label))
            .push(button::standard("Cancel").on_press(Message::Cancel))
            .push(button::suggested("Record").on_press(Message::Confirm))
            .spacing(spacing.space_s)
            .align_y(Alignment::Center);

        // Inner container: the toolbar itself, styled like a COSMIC dialog.
        // Outer container: fills the screen and pushes the toolbar to the bottom.
        container(
            container(bar)
                .padding(spacing.space_s)
                .class(cosmic::theme::Container::Dialog(true)),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::End)
        .padding(spacing.space_xl)
        .into()
    }
}

impl cosmic::Application for Selector {
    type Executor = cosmic::executor::Default;
    type Flags = mpsc::Sender<Option<Region>>;
    type Message = Message;
    const APP_ID: &'static str = "com.github.cosmic-screencast";

    fn core(&self) -> &app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut app::Core {
        &mut self.core
    }

    fn init(core: app::Core, result: Self::Flags) -> (Self, Task<Message>) {
        let surface = window::Id::unique();
        let app = Selector {
            core,
            result,
            surface,
            rect: None,
            recording: false,
        };
        // Ask the compositor for a fullscreen overlay on the active output.
        let task = get_layer_surface(SctkLayerSurfaceSettings {
            id: surface,
            layer: Layer::Overlay,
            // Exclusive: we get all keyboard input (for Escape / Enter).
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            anchor: Anchor::all(),
            output: IcedOutput::Active,
            namespace: "cosmic-screencast".into(),
            size: Some((None, None)),
            // -1: ignore panels/docks, cover the whole screen.
            exclusive_zone: -1,
            ..Default::default()
        });
        (app, task)
    }

    fn subscription(&self) -> Subscription<Message> {
        // Keyboard shortcuts, wherever the focus is.
        event::listen_with(|event, _status, _window| match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(named),
                ..
            }) => match named {
                Named::Escape => Some(Message::Cancel),
                Named::Enter => Some(Message::Confirm),
                _ => None,
            },
            _ => None,
        })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        if self.recording {
            return Task::none();
        }
        match message {
            Message::Changed(rect) => {
                self.rect = Some(rect);
                Task::none()
            }
            Message::Confirm => self.confirm(),
            // Exit without sending anything.
            Message::Cancel => cosmic::iced::exit(),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        // Never called: we have no main window.
        unreachable!()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        // `stack!` draws its children on top of each other; the last one is
        // on top and receives mouse events first.
        let selection = Selection::new(self.rect, Message::Changed).locked(self.recording);
        if self.recording {
            // No toolbar: it could be inside the recorded region.
            return selection.into();
        }
        cosmic::iced::widget::stack![selection, self.toolbar()].into()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        // Transparent background, so the desktop stays visible under the overlay.
        let theme = cosmic::theme::active();
        let text = Color::from(theme.cosmic().on_bg_color());
        Some(cosmic::iced::theme::Style {
            background_color: Color::TRANSPARENT,
            text_color: text,
            icon_color: text,
        })
    }
}
