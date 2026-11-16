//! The nav's brand mark: the Ninox owl's head from the app icon's line art
//! (`assets/icon-1024.png`), drawn as vectors so it stays crisp at 14px.

use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Theme};

use crate::app::Message;

/// The icon's gold.
pub const OWL_GOLD: Color = Color { r: 0xd4 as f32 / 255.0, g: 0xa8 as f32 / 255.0, b: 0x4f as f32 / 255.0, a: 1.0 };
const SIZE: f32 = 14.0;

struct OwlMark;

impl canvas::Program<Message> for OwlMark {
    type State = ();

    fn draw(&self, _state: &(), renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let s = bounds.width.min(bounds.height) / SIZE;
        let p = |x: f32, y: f32| Point::new(x * s, y * s);
        let line = Stroke::default().with_color(OWL_GOLD).with_width(0.9 * s);

        // Head: the icon's flat brow between two corner ear tufts, tapering to a point.
        let head = Path::new(|b| {
            b.move_to(p(1.2, 0.8));
            b.line_to(p(4.4, 2.4));
            b.line_to(p(9.6, 2.4));
            b.line_to(p(12.8, 0.8));
            b.line_to(p(12.8, 8.6));
            b.line_to(p(7.0, 13.2));
            b.line_to(p(1.2, 8.6));
            b.close();
        });
        frame.stroke(&head, line);
        for cx in [4.6, 9.4] {
            frame.stroke(&Path::circle(p(cx, 5.5), 1.9 * s), line);
            frame.fill(&Path::circle(p(cx, 5.5), 0.8 * s), OWL_GOLD);
        }
        let beak = Path::new(|b| {
            b.move_to(p(6.2, 8.2));
            b.line_to(p(7.8, 8.2));
            b.line_to(p(7.0, 10.0));
            b.close();
        });
        frame.fill(&beak, OWL_GOLD);
        vec![frame.into_geometry()]
    }
}

pub fn owl_mark<'a>() -> Element<'a, Message> {
    canvas::Canvas::new(OwlMark).width(Length::Fixed(SIZE)).height(Length::Fixed(SIZE)).into()
}
