//! A custom iced widget to draw / move / resize a rectangle with the mouse.
//!
//! Its look follows the COSMIC screenshot portal: the screen is dimmed except
//! inside the selection, which gets an accent-colored border and corner handles.
//!
//! An iced widget doesn't own the application data: it receives the current
//! rectangle from the app, and *publishes* a message when the user changes it.
//! The app then updates its state and rebuilds the view with the new rectangle.

use cosmic::iced::core::layout::{Layout, Limits, Node};
use cosmic::iced::core::renderer::{Quad, Renderer as _};
use cosmic::iced::core::widget::{Tree, tree};
use cosmic::iced::core::{
    Border, Clipboard, Color, Element, Event, Length, Point, Rectangle, Shell, Size, Vector,
    Widget, mouse,
};

/// Size of the corner handles, in logical pixels.
const HANDLE_SIZE: f32 = 16.0;
/// Width of the selection border.
const BORDER_WIDTH: f32 = 4.0;
/// Opacity of the black layer dimming the non-selected area.
const DIM_ALPHA: f32 = 0.3;

pub struct Selection<'a, Message> {
    rect: Option<Rectangle>,
    on_change: Box<dyn Fn(Rectangle) -> Message + 'a>,
}

impl<'a, Message> Selection<'a, Message> {
    pub fn new(rect: Option<Rectangle>, on_change: impl Fn(Rectangle) -> Message + 'a) -> Self {
        Self {
            rect,
            on_change: Box::new(on_change),
        }
    }
}

/// What the mouse is currently doing. iced stores this in the widget "tree",
/// so it survives between frames (the widget itself is rebuilt every time).
#[derive(Default)]
enum Drag {
    #[default]
    Idle,
    /// Drawing or resizing: the rectangle goes from `anchor` to the cursor.
    Resizing { anchor: Point },
    /// Moving the whole rectangle; `grab` is the cursor offset from its top-left.
    Moving { grab: Vector },
}

/// Builds the rectangle between two points, whatever their order.
fn rect_between(a: Point, b: Point) -> Rectangle {
    let top_left = Point::new(a.x.min(b.x), a.y.min(b.y));
    let size = Size::new((a.x - b.x).abs(), (a.y - b.y).abs());
    Rectangle::new(top_left, size)
}

fn corners(r: Rectangle) -> [Point; 4] {
    [
        Point::new(r.x, r.y),
        Point::new(r.x + r.width, r.y),
        Point::new(r.x + r.width, r.y + r.height),
        Point::new(r.x, r.y + r.height),
    ]
}

/// Returns the index of the corner under `p`, if any.
fn corner_at(r: Rectangle, p: Point) -> Option<usize> {
    corners(r)
        .iter()
        .position(|c| (c.x - p.x).abs() <= HANDLE_SIZE && (c.y - p.y).abs() <= HANDLE_SIZE)
}

/// Clamps a point inside `bounds` (the size of the screen).
fn clamp_point(p: Point, bounds: Size) -> Point {
    Point::new(p.x.clamp(0.0, bounds.width), p.y.clamp(0.0, bounds.height))
}

// The `Widget` trait is generic over Message, Theme and Renderer. We only
// support COSMIC's theme and renderer, which gives us access to the accent color.
impl<Message> Widget<Message, cosmic::Theme, cosmic::Renderer> for Selection<'_, Message> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(&mut self, _tree: &mut Tree, _renderer: &cosmic::Renderer, limits: &Limits) -> Node {
        Node::new(limits.max())
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Drag>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Drag::default())
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &cosmic::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let drag = tree.state.downcast_mut::<Drag>();
        let bounds = layout.bounds();
        // Cursor position relative to the widget (= to the screen).
        let Some(pos) = cursor.position_in(bounds) else {
            return;
        };

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let corner = self.rect.and_then(|r| corner_at(r, pos));
                *drag = match (self.rect, corner) {
                    // Grab a corner: the opposite corner becomes the anchor.
                    (Some(r), Some(i)) => Drag::Resizing {
                        anchor: corners(r)[(i + 2) % 4],
                    },
                    // Grab the inside: move the rectangle.
                    (Some(r), None) if r.contains(pos) => Drag::Moving {
                        grab: pos - Point::new(r.x, r.y),
                    },
                    // Click outside: start a new rectangle.
                    _ => {
                        shell.publish((self.on_change)(rect_between(pos, pos)));
                        Drag::Resizing { anchor: pos }
                    }
                };
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => match *drag {
                Drag::Idle => {}
                Drag::Resizing { anchor } => {
                    let p = clamp_point(pos, bounds.size());
                    shell.publish((self.on_change)(rect_between(anchor, p)));
                }
                Drag::Moving { grab } => {
                    if let Some(r) = self.rect {
                        // Keep the rectangle fully on screen.
                        let x = (pos.x - grab.x).clamp(0.0, bounds.width - r.width);
                        let y = (pos.y - grab.y).clamp(0.0, bounds.height - r.height);
                        shell.publish((self.on_change)(Rectangle::new(Point::new(x, y), r.size())));
                    }
                }
            },
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                *drag = Drag::Idle;
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        let Some(pos) = cursor.position_in(layout.bounds()) else {
            return mouse::Interaction::None;
        };
        match (tree.state.downcast_ref::<Drag>(), self.rect) {
            (Drag::Moving { .. }, _) => mouse::Interaction::Grabbing,
            (Drag::Resizing { .. }, _) => mouse::Interaction::Crosshair,
            (Drag::Idle, Some(r)) => match corner_at(r, pos) {
                // Corners 0 and 2 are top-left / bottom-right: "\" diagonal.
                Some(0 | 2) => mouse::Interaction::ResizingDiagonallyDown,
                Some(_) => mouse::Interaction::ResizingDiagonallyUp,
                None if r.contains(pos) => mouse::Interaction::Grab,
                None => mouse::Interaction::Crosshair,
            },
            (Drag::Idle, None) => mouse::Interaction::Crosshair,
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        _style: &cosmic::iced::core::renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let dim = Color {
            a: DIM_ALPHA,
            ..Color::BLACK
        };
        let fill = |renderer: &mut cosmic::Renderer, r: Rectangle, color: Color| {
            renderer.fill_quad(
                Quad {
                    bounds: r,
                    ..Default::default()
                },
                color,
            );
        };

        let Some(sel) = self.rect else {
            // Nothing selected yet: dim the whole screen.
            fill(renderer, bounds, dim);
            return;
        };

        // Dim around the selection with 4 bands: top, bottom, left, right.
        let (w, h) = (bounds.width, bounds.height);
        let bottom = sel.y + sel.height;
        let right = sel.x + sel.width;
        fill(
            renderer,
            Rectangle::new(Point::ORIGIN, Size::new(w, sel.y)),
            dim,
        );
        fill(
            renderer,
            Rectangle::new(Point::new(0.0, bottom), Size::new(w, h - bottom)),
            dim,
        );
        fill(
            renderer,
            Rectangle::new(Point::new(0.0, sel.y), Size::new(sel.x, sel.height)),
            dim,
        );
        fill(
            renderer,
            Rectangle::new(Point::new(right, sel.y), Size::new(w - right, sel.height)),
            dim,
        );

        let cosmic = theme.cosmic();
        let accent = Color::from(cosmic.accent_color());

        // Selection border (drawn inside the rectangle).
        renderer.fill_quad(
            Quad {
                bounds: sel,
                border: Border {
                    width: BORDER_WIDTH,
                    color: accent,
                    radius: 0.0.into(),
                },
                ..Default::default()
            },
            Color::TRANSPARENT,
        );

        // Corner handles.
        for c in corners(sel) {
            renderer.fill_quad(
                Quad {
                    bounds: Rectangle::new(
                        Point::new(c.x - HANDLE_SIZE / 2.0, c.y - HANDLE_SIZE / 2.0),
                        Size::new(HANDLE_SIZE, HANDLE_SIZE),
                    ),
                    border: Border {
                        radius: cosmic.radius_s().into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                accent,
            );
        }
    }
}

// Lets us write `Selection::new(...).into()` wherever an Element is expected.
impl<'a, Message: 'a> From<Selection<'a, Message>>
    for Element<'a, Message, cosmic::Theme, cosmic::Renderer>
{
    fn from(selection: Selection<'a, Message>) -> Self {
        Element::new(selection)
    }
}
