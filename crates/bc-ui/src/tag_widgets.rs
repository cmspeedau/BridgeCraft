//! Star rating, color label, flag and keyword widgets for the metadata panel
//! (the editor issue #10 left unbuilt).
//!
//! Split from `app.rs` so the panel stays a thin caller and the geometry and
//! the states can be tested without a window.

use bc_metadata::ratings::Flag;
use egui::{Color32, Pos2, Shape, Stroke, Ui, Vec2};

use crate::metadata_edit::{is_known_label, label_color, LABEL_CHOICES};

/// The shell's one accent, the same color `app.rs` marks selection with. A
/// filled star is the strongest "this is my rating" mark the panel can make,
/// and a second accent for it would break the chrome palette.
pub const ACCENT: Color32 = Color32::from_rgb(0, 160, 255);

/// Edge length of one star in the rating row, in points.
pub const STAR_SIZE: f32 = 16.0;

/// The ten vertices of a five-pointed star: outer points at `radius`, inner
/// points at 42% of it, first spike pointing up.
pub fn star_points(center: Pos2, radius: f32) -> [Pos2; 10] {
    let inner = radius * 0.42;
    let mut points = [Pos2::ZERO; 10];
    let step = std::f32::consts::TAU / 10.0;
    for (i, point) in points.iter_mut().enumerate() {
        let radius_i = if i % 2 == 0 { radius } else { inner };
        let angle = -std::f32::consts::FRAC_PI_2 + i as f32 * step;
        *point = center + Vec2::new(angle.cos(), angle.sin()) * radius_i;
    }
    points
}

/// A star as convex pieces: a centre pentagon and five spike triangles, plus
/// the ten-point outline.
///
/// egui 0.27 only fills convex polygons reliably and a star is not convex, so
/// it is decomposed. An outline alone would leave the middle hollow and read
/// as a checkbox rather than a rating.
pub fn star_shapes(center: Pos2, radius: f32, fill: Color32, stroke: Stroke) -> Vec<Shape> {
    let points = star_points(center, radius);
    let mut shapes = vec![Shape::convex_polygon(
        vec![points[1], points[3], points[5], points[7], points[9]],
        fill,
        Stroke::NONE,
    )];
    for spike in [0usize, 2, 4, 6, 8] {
        shapes.push(Shape::convex_polygon(
            vec![
                center,
                points[(spike + 9) % 10],
                points[spike],
                points[(spike + 1) % 10],
            ],
            fill,
            Stroke::NONE,
        ));
    }
    shapes.push(Shape::Path(egui::epaint::PathShape {
        points: points.to_vec(),
        closed: true,
        fill: Color32::TRANSPARENT,
        stroke,
    }));
    shapes
}

/// Five clickable stars. `stars` is the current rating; the returned index is
/// the one the user pressed, and the caller decides what that means (clicking
/// the current rating clears it, which is form logic, not widget logic).
///
/// Each star is a real button whose label is the count, drawn in a transparent
/// color: the accessible name and the keyboard focus belong to the button,
/// while the visible mark is the shape painted inside it.
pub fn rating_row(ui: &mut Ui, stars: u8) -> Option<u8> {
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        for index in 1..=5u8 {
            let filled = index <= stars;
            let name = format!("{index} star{}", if index == 1 { "" } else { "s" });
            let response = ui.add(
                egui::Button::new(egui::RichText::new(name.clone()).color(Color32::TRANSPARENT))
                    .min_size(Vec2::splat(STAR_SIZE)),
            );
            let stroke = if filled {
                Stroke::new(1.0_f32, ACCENT)
            } else {
                Stroke::new(1.0_f32, ui.visuals().weak_text_color())
            };
            let fill = if filled { ACCENT } else { Color32::TRANSPARENT };
            for shape in star_shapes(response.rect.center(), STAR_SIZE * 0.46, fill, stroke) {
                ui.painter().add(shape);
            }
            if response.on_hover_text(format!("Rate {name}")).clicked() {
                picked = Some(index);
            }
        }
    });
    picked
}

/// A compact star count for a cell or a row: the filled stars only, at a size
/// that stays readable at 7 points. Drawing the same shape as the panel keeps
/// one visual language for one piece of data, and a "3/5" label would read as
/// a counter rather than a rating.
pub fn rating_inline(ui: &mut Ui, at: Pos2, stars: u8) {
    let size = 7.0_f32;
    for index in 0..stars.min(5) {
        let center = Pos2::new(at.x + size * (index as f32 + 0.5), at.y + size * 0.5);
        for shape in star_shapes(center, size * 0.46, ACCENT, Stroke::new(0.7_f32, ACCENT)) {
            ui.painter().add(shape);
        }
    }
}

/// The fixed label chips, plus a chip for any label name another tool wrote.
/// The selected chip is ringed *and* its name is bold, so its state never rests
/// on color alone.
pub fn label_chips(ui: &mut Ui, current: &str) -> Option<String> {
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (name, color) in LABEL_CHOICES {
            let selected = *name == current;
            let text = if name.is_empty() { "None" } else { name };
            let rich = if selected {
                egui::RichText::new(text.to_string()).small().strong()
            } else {
                egui::RichText::new(text.to_string()).small()
            };
            let response = ui.add(egui::Button::new(rich).min_size(Vec2::new(0.0, 20.0)));
            let center = Pos2::new(response.rect.left() + 9.0, response.rect.center().y);
            if name.is_empty() {
                ui.painter()
                    .circle_stroke(center, 3.6, Stroke::new(1.2_f32, *color));
            } else {
                ui.painter().circle_filled(center, 3.6, *color);
            }
            if selected {
                ui.painter().rect_stroke(
                    response.rect.expand(1.0),
                    ui.visuals().widgets.noninteractive.rounding,
                    Stroke::new(1.5_f32, ACCENT),
                );
            }
            if response.clicked() {
                picked = Some((*name).to_string());
            }
        }
        if !current.is_empty() && !is_known_label(current) {
            // A label another tool wrote: show it, so the user can clear it
            // instead of staring at eight chips that are all wrong.
            let rich = egui::RichText::new(current.to_string()).small().strong();
            let response = ui.add(egui::Button::new(rich).min_size(Vec2::new(0.0, 20.0)));
            let center = Pos2::new(response.rect.left() + 9.0, response.rect.center().y);
            ui.painter()
                .circle_filled(center, 3.6, label_color(current));
            ui.painter().rect_stroke(
                response.rect.expand(1.0),
                ui.visuals().widgets.noninteractive.rounding,
                Stroke::new(1.5_f32, label_color(current)),
            );
            if response
                .on_hover_text("Label written by another tool. Click to clear it.")
                .clicked()
            {
                picked = Some(String::new());
            }
        }
    });
    picked
}

/// Approve, reject, clear. Text buttons rather than tick and cross glyphs: the
/// words are the accessible names, and two near-identical checkmarks would be
/// told apart only by color, which rule R-39 forbids.
pub fn flag_row(ui: &mut Ui, current: Flag) -> Option<Flag> {
    let mut picked = None;
    ui.horizontal(|ui| {
        for (text, flag) in [
            ("Approve", Flag::Approved),
            ("Reject", Flag::Rejected),
            ("No flag", Flag::None),
        ] {
            let selected = current == flag;
            let rich = if selected {
                egui::RichText::new(text).small().strong()
            } else {
                egui::RichText::new(text).small()
            };
            let response = ui.add(egui::Button::new(rich).min_size(Vec2::new(0.0, 20.0)));
            if selected {
                ui.painter().rect_stroke(
                    response.rect.expand(1.0),
                    ui.visuals().widgets.noninteractive.rounding,
                    Stroke::new(1.5_f32, ACCENT),
                );
            }
            if response.clicked() {
                picked = Some(flag);
            }
        }
    });
    picked
}

/// Keyword chips. The chip itself is the remove control and the panel heading
/// says so, which keeps each chip's accessible name the keyword rather than an
/// unreadable "times" glyph. The empty state names the next action instead of
/// leaving a bare box (R-27).
pub fn keyword_chips(ui: &mut Ui, keywords: &[String]) -> Option<String> {
    let mut removed = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if keywords.is_empty() {
            ui.add(
                egui::Label::new(
                    egui::RichText::new("No keywords yet. Type one below and press Enter.")
                        .weak()
                        .small(),
                )
                .selectable(false),
            );
        }
        for keyword in keywords {
            let response = ui.add(
                egui::Button::new(egui::RichText::new(keyword.as_str()).small())
                    .min_size(Vec2::new(0.0, 20.0)),
            );
            if response
                .on_hover_text("Click to remove this keyword")
                .clicked()
            {
                removed = Some(keyword.clone());
            }
        }
    });
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_points_alternate_outer_and_inner_with_the_first_spike_up() {
        let center = Pos2::new(50.0, 50.0);
        let radius = 20.0;
        let points = star_points(center, radius);
        for (i, point) in points.iter().enumerate() {
            let distance = (*point - center).length();
            let expected = if i % 2 == 0 { radius } else { radius * 0.42 };
            assert!(
                (distance - expected).abs() < 1e-3,
                "vertex {i} is {distance} from the centre, expected {expected}"
            );
        }
        // The first spike points up, not down or sideways.
        assert!(points[0].y < center.y - radius * 0.9);
        assert!((points[0].x - center.x).abs() < 1e-3);
    }

    #[test]
    fn a_star_is_built_from_convex_pieces_only() {
        // egui 0.27 fills convex polygons; the decomposition must never hand it
        // a shape whose fill would be tessellated wrong. Every filled piece here
        // is a closed `PathShape` of 4 or 5 points, all convex by construction.
        let shapes = star_shapes(
            Pos2::new(0.0, 0.0),
            10.0,
            ACCENT,
            Stroke::new(1.0_f32, ACCENT),
        );
        assert_eq!(shapes.len(), 7, "one pentagon, five spikes, one outline");
        for (index, shape) in shapes.iter().enumerate() {
            match shape {
                Shape::Path(path) => {
                    assert!(
                        path.closed,
                        "piece {index} must be closed or its fill is undefined"
                    );
                    let expected = if index == 0 {
                        5
                    } else if index < 6 {
                        4
                    } else {
                        10
                    };
                    assert_eq!(
                        path.points.len(),
                        expected,
                        "unexpected piece {index} with {} points",
                        path.points.len()
                    );
                    let filled = path.fill != Color32::TRANSPARENT;
                    let outline_only = index == shapes.len() - 1;
                    assert_eq!(
                        filled, !outline_only,
                        "piece {index} fill does not match its role"
                    );
                }
                other => panic!("expected a path piece at {index}, got {other:?}"),
            }
        }
    }
}
