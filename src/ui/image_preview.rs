//! Image comparison using the same renderer in terminal and window modes.
use iced_core::image::Renderer as _;
use iced_core::{
    image, layout, mouse, renderer, widget::Tree, Color, Length, Rectangle, Size, Widget,
};
use iced_widget::{column, container, responsive, row, text};

use super::app::{App, Element, Message, Renderer};
use super::log::fill;
use crate::git::images::{ImageComparison, ImageSide, Preview};
use crate::git::repo::DiffTarget;

pub fn view<'a>(app: &'a App, images: &'a ImageComparison) -> Element<'a> {
    let (before_label, after_label) = match app.selected_file {
        Some(DiffTarget::WorkdirUnstaged(_)) => ("Before · Index", "After · Working tree"),
        Some(DiffTarget::Staged(_)) => ("Before · HEAD", "After · Index"),
        _ => ("Before · Parent", "After · Commit"),
    };
    responsive(move |size| {
        let before = panel(app, before_label, &images.before);
        let after = panel(app, after_label, &images.after);
        if size.width < 520.0 {
            column![before, after]
                .spacing(12)
                .padding(12)
                .height(Length::Fill)
                .into()
        } else {
            row![before, after]
                .spacing(12)
                .padding(12)
                .height(Length::Fill)
                .into()
        }
    })
    .into()
}

fn panel<'a>(app: &'a App, label: &'static str, side: &'a ImageSide) -> Element<'a> {
    let mut content = column![text(label).size(12).color(app.theme.strong)].spacing(6);
    match side {
        ImageSide::Ready(preview) => {
            content = content.push(
                text(format!(
                    "{} × {} px · {:.1} KB",
                    preview.width,
                    preview.height,
                    preview.bytes as f64 / 1024.0
                ))
                .size(11)
                .color(app.theme.weak),
            );
            content = content.push(iced_core::Element::new(ImageCanvas(preview)));
        }
        ImageSide::Missing => {
            content = content.push(
                container(text("File not present").size(12).color(app.theme.weak))
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
            );
        }
        ImageSide::Error(error) => {
            content = content.push(
                container(text(error).size(12).color(app.theme.error))
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .padding(12),
            );
        }
    }
    content.width(Length::Fill).height(Length::Fill).into()
}

struct ImageCanvas<'a>(&'a Preview);

impl Widget<Message, iced_core::Theme, Renderer> for ImageCanvas<'_> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &iced_core::Theme,
        _style: &renderer::Style,
        layout: layout::Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let area = layout.bounds();
        if area.width <= 0.0 || area.height <= 0.0 {
            return;
        }
        let factor = (area.width / self.0.width.max(1) as f32)
            .min(area.height / self.0.height.max(1) as f32)
            .min(1.0);
        let width = self.0.width as f32 * factor;
        let height = self.0.height as f32 * factor;
        let bounds = Rectangle::new(
            iced_core::Point::new(
                area.x + (area.width - width) / 2.0,
                area.y + (area.height - height) / 2.0,
            ),
            Size::new(width, height),
        );
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        // Keep checker cells neutral so transparency stays clear in either theme.
        for y in 0..(height / 12.0).ceil() as u32 {
            for x in 0..(width / 12.0).ceil() as u32 {
                let cell = Rectangle {
                    x: bounds.x + x as f32 * 12.0,
                    y: bounds.y + y as f32 * 12.0,
                    width: 12.0_f32.min(width - x as f32 * 12.0),
                    height: 12.0_f32.min(height - y as f32 * 12.0),
                };
                if let Some(cell) = cell.intersection(&clip) {
                    let shade = if (x + y) % 2 == 0 { 0.82 } else { 0.94 };
                    fill(renderer, cell, Color::from_rgb(shade, shade, shade), 0.0);
                }
            }
        }
        renderer.draw_image(image::Image::new(self.0.handle.clone()), bounds, clip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{
        ops::Reply,
        repo::{testutil::TempRepo, Repo},
    };
    use crate::ui::{app::Pane, theme::Theme};
    use crate::{render::frame::Framebuffer, runtime::settle, shell::Shell};

    #[test]
    fn previews_render_in_wide_and_narrow_panes_and_source_toggle_works() {
        let t = TempRepo::new();
        let art = |color| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="240" height="140"><rect x="10" y="10" width="220" height="120" rx="24" fill="{color}"/></svg>"#
            )
        };
        t.commit_file("art.svg", &art("red"), "Add artwork");
        t.write("art.svg", &art("blue"));
        let mut repo = Repo::open(&t.dir).unwrap();
        let theme = Theme::dark();
        let mut shell = Shell::new(13.0, 1.0, 1200, 800, theme.iced());
        let mut app = App::new(theme, "test", 1.0, t.dir.clone());
        app.apply(Reply::Snapshot(repo.snapshot(100).unwrap()));
        settle(&mut app, &mut repo);
        for (w, h) in [(1200, 800), (440, 800)] {
            app.update(Message::WindowResized(Size::new(w as f32, h as f32)));
            let detail = app.pane_of(Pane::Detail).unwrap();
            app.update(Message::PaneMaximize(detail));
            let mut fb = Framebuffer::new(w, h);
            shell.resize(w, h, 1.0);
            for _ in 0..3 {
                shell.frame(&mut app, &mut fb);
            }
            let red = fb
                .pixels()
                .chunks_exact(4)
                .filter(|p| p[0] > 245 && p[1] < 10 && p[2] < 10)
                .count();
            let blue = fb
                .pixels()
                .chunks_exact(4)
                .filter(|p| p[2] > 245 && p[1] < 10 && p[0] < 10)
                .count();
            fb.save_png(&std::env::temp_dir().join(format!("gitgui-image-preview-{w}.png")))
                .unwrap();
            assert!(
                red > 1000 && blue > 1000,
                "both image sides should render at {w}px: red {red}, blue {blue}"
            );
            let preview_pixels = fb.pixels().to_vec();
            app.update(Message::DiffImageSource);
            assert!(app.image_source);
            for _ in 0..3 {
                shell.frame(&mut app, &mut fb);
            }
            assert_ne!(preview_pixels, fb.pixels());
            app.update(Message::DiffImageSource);
            assert!(!app.image_source);
        }
        app.update(Message::DiffSearchOpen);
        assert!(app.image_source, "search reveals SVG source");
        app.select_file(Some(DiffTarget::Staged("another.svg".into())));
        assert!(!app.image_source, "new selection defaults to preview");
    }
}
