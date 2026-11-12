//! The region selection overlay: a small libcosmic application whose only
//! surface is a fullscreen, transparent "layer shell" surface on top of
//! everything (like the screenshot portal), with no regular window.

use std::sync::mpsc;

use anyhow::{Result, anyhow};
use cosmic::Element;
use cosmic::app::{self, Task};
use cosmic::cctk::sctk::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
use cosmic::iced::core::keyboard::{self, key::Named};
use cosmic::iced::platform_specific::shell::commands::layer_surface::get_layer_surface;
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::{
    IcedOutput, SctkLayerSurfaceSettings,
};
use cosmic::iced::{Alignment, Color, Event, Length, Rectangle, Subscription, event, window};
use cosmic::widget::{Row, button, container, text};

use crate::region::Region;
use crate::selection::Selection;

/// Shows the overlay and blocks until the user confirms or cancels.
/// Returns `None` if cancelled.
pub fn run() -> Result<Option<Region>> {
    // The app can't return a value, so it sends the result through a channel.
    let (tx, rx) = mpsc::channel();
    let settings = app::Settings::default()
        .no_main_window(true)
        .exit_on_close(false);
    cosmic::app::run::<Selector>(settings, tx).map_err(|e| anyhow!("overlay failed: {e}"))?;
    // `try_recv` fails if nothing was sent: that's a cancel.
    Ok(rx.try_recv().ok())
}

struct Selector {
    core: app::Core,
    result: mpsc::Sender<Region>,
    surface: window::Id,
    /// Size of the overlay = size of the screen, in logical pixels.
    screen: Option<(f32, f32)>,
    rect: Option<Rectangle>,
}

#[derive(Debug, Clone)]
enum Message {
    Changed(Rectangle),
    Confirm,
    Cancel,
}

impl Selector {
    fn confirm(&mut self) -> Task<Message> {
        let region = match (self.rect, self.screen) {
            // A tiny rectangle is probably a misclick: ignore it.
            (Some(r), _) if r.width >= 8.0 && r.height >= 8.0 => Region {
                x: r.x.round() as u32,
                y: r.y.round() as u32,
                width: r.width.round() as u32,
                height: r.height.round() as u32,
            },
            // No selection: record the whole screen.
            (None, Some((w, h))) => Region {
                x: 0,
                y: 0,
                width: w as u32,
                height: h as u32,
            },
            _ => return Task::none(),
        };
        let _ = self.result.send(region);
        cosmic::iced::exit()
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
    type Flags = mpsc::Sender<Region>;
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
            screen: None,
            rect: None,
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

    fn on_window_resize(&mut self, id: window::Id, width: f32, height: f32) {
        if id == self.surface {
            self.screen = Some((width, height));
        }
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
        cosmic::iced::widget::stack![Selection::new(self.rect, Message::Changed), self.toolbar(),]
            .into()
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
