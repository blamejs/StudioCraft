//! A focusable divider that reports pointer, arrow-key and accessibility resizing.

// port: P11 (dock strip and floating docks) replaces this copy of the resize handle that craft-ui
// dropped with the framework's divider.

use std::ops::RangeInclusive;

use egui::{Color32, EventFilter, Id, Key, Modifiers, Rect, Response, Sense, Ui, Vec2, accesskit};

/// The direction a handle moves in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAxis {
    /// The handle moves left and right.
    Horizontal,
    /// The handle moves up and down.
    Vertical,
}

/// Colors for a resize handle. The ordinary, unfocused handle does not paint a fill.
#[derive(Clone, Copy, Debug)]
pub struct ResizeHandleStyle {
    pub hovered: Color32,
    pub dragged: Color32,
    pub focused: Color32,
}

/// An interaction proposal; the caller applies `delta` using its own minimum-size policy.
pub struct ResizeResponse {
    /// Includes the ordinary egui drag-start/stop and focus lifecycle.
    pub response: Response,
    /// Requested movement this frame in logical points along the split axis.
    pub delta: f32,
}

/// A focusable divider with pointer, arrow-key and accessibility resizing. It never stores or
/// changes a pane size.
pub struct ResizeHandle<'a> {
    id: Id,
    axis: SplitAxis,
    rect: Rect,
    label: &'a str,
    style: ResizeHandleStyle,
    hit_margin: f32,
    step: f32,
    value: Option<(f32, RangeInclusive<f32>)>,
    reversed: bool,
}

fn valid_rect(rect: Rect) -> bool {
    rect.min.is_finite()
        && rect.max.is_finite()
        && rect.width().is_finite()
        && rect.height().is_finite()
        && rect.min.x <= rect.max.x
        && rect.min.y <= rect.max.y
}

impl<'a> ResizeHandle<'a> {
    pub fn new(id: Id, axis: SplitAxis, rect: Rect, label: &'a str, style: ResizeHandleStyle) -> Self {
        Self { id, axis, rect, label, style, hit_margin: 3.0, step: 8.0, value: None, reversed: false }
    }

    /// Expand the hit rectangle along the movement axis, in logical points. Default: 3.
    pub fn hit_margin(mut self, margin: f32) -> Self {
        self.hit_margin = margin;
        self
    }

    /// Expose the current size and bounds to accessibility, and support its SetValue action.
    pub fn value(mut self, value: f32, range: RangeInclusive<f32>) -> Self {
        self.value = Some((value, range));
        self
    }

    /// The value grows against the axis. Accessibility actions then work on the value; the
    /// pointer and arrow keys stay physical, and `delta` stays along the axis.
    pub fn reversed(mut self, reversed: bool) -> Self {
        self.reversed = reversed;
        self
    }

    /// Show the handle and return a resize proposal for the caller to apply. Invalid geometry or
    /// values make the handle inert.
    pub fn show(self, ui: &mut Ui) -> ResizeResponse {
        let hit = self.rect.expand2(match self.axis {
            SplitAxis::Horizontal => Vec2::new(self.hit_margin, 0.0),
            SplitAxis::Vertical => Vec2::new(0.0, self.hit_margin),
        });
        let valid = valid_rect(self.rect)
            && self.rect.width() > 0.0
            && self.rect.height() > 0.0
            && valid_rect(hit)
            && self.hit_margin.is_finite()
            && (0.0..=4096.0).contains(&self.hit_margin)
            && self.step.is_finite()
            && self.step > 0.0
            && self.value.as_ref().is_none_or(|(value, range)| {
                value.is_finite() && range.start().is_finite() && range.end().is_finite() && range.start() <= range.end()
            });
        let inert = Rect::from_min_size(ui.min_rect().min, Vec2::ZERO);
        let mut response = ui.interact(if valid { hit } else { inert }, self.id, if valid { Sense::drag() } else { Sense::hover() });
        let enabled = valid && response.enabled();
        let mut delta = 0.0;
        if enabled {
            drag_focus(ui, &response);
            if response.dragged() {
                let drag = response.drag_delta();
                delta = match self.axis {
                    SplitAxis::Horizontal => drag.x,
                    SplitAxis::Vertical => drag.y,
                };
            }
            if response.has_focus() {
                ui.memory_mut(|memory| {
                    memory.set_focus_lock_filter(
                        response.id,
                        EventFilter {
                            horizontal_arrows: self.axis == SplitAxis::Horizontal,
                            vertical_arrows: self.axis == SplitAxis::Vertical,
                            ..Default::default()
                        },
                    );
                });
                let (back, forward) = match self.axis {
                    SplitAxis::Horizontal => (Key::ArrowLeft, Key::ArrowRight),
                    SplitAxis::Vertical => (Key::ArrowUp, Key::ArrowDown),
                };
                ui.input_mut(|input| {
                    if input.consume_key(Modifiers::NONE, back) {
                        delta -= self.step;
                    }
                    if input.consume_key(Modifiers::NONE, forward) {
                        delta += self.step;
                    }
                });
            }
            let mut change = 0.0;
            ui.input_mut(|input| {
                input.consume_accesskit_action_requests(response.id, |request| match request.action {
                    accesskit::Action::Increment => {
                        change += self.step;
                        true
                    }
                    accesskit::Action::Decrement => {
                        change -= self.step;
                        true
                    }
                    accesskit::Action::SetValue => {
                        if let (Some((value, range)), Some(accesskit::ActionData::NumericValue(new_value))) = (&self.value, &request.data)
                            && new_value.is_finite()
                        {
                            change = (new_value.clamp(f64::from(*range.start()), f64::from(*range.end())) - f64::from(*value)) as f32;
                            delta = 0.0;
                        }
                        true
                    }
                    _ => false,
                });
            });
            delta += if self.reversed { -change } else { change };
        }
        if !delta.is_finite() {
            delta = 0.0;
        }
        if delta != 0.0 {
            response.mark_changed();
        }
        response.widget_info(|| {
            if let Some((value, _)) = &self.value
                && valid
            {
                egui::WidgetInfo::slider(enabled, f64::from(*value), self.label)
            } else {
                egui::WidgetInfo::labeled(egui::Role::Unknown, enabled, self.label)
            }
        });
        ui.ctx().accesskit_node_builder(response.id, |node| {
            node.set_role(accesskit::Role::Splitter);
            // Orientation describes the divider itself, perpendicular to its movement axis.
            node.set_orientation(match self.axis {
                SplitAxis::Horizontal => accesskit::Orientation::Vertical,
                SplitAxis::Vertical => accesskit::Orientation::Horizontal,
            });
            if enabled {
                node.add_action(accesskit::Action::Increment);
                node.add_action(accesskit::Action::Decrement);
                if let Some((value, range)) = &self.value {
                    node.set_numeric_value(f64::from(*value));
                    node.set_min_numeric_value(f64::from(*range.start()));
                    node.set_max_numeric_value(f64::from(*range.end()));
                    node.set_numeric_value_step(f64::from(self.step));
                    node.add_action(accesskit::Action::SetValue);
                }
            }
        });
        if enabled {
            let fill = if response.dragged() {
                Some(self.style.dragged)
            } else if response.has_focus() {
                Some(self.style.focused)
            } else if response.hovered() {
                Some(self.style.hovered)
            } else {
                None
            };
            if let Some(fill) = fill {
                ui.painter().rect_filled(self.rect, 0.0, fill);
            }
            if response.hovered() || response.dragged() {
                ui.ctx().set_cursor_icon(match self.axis {
                    SplitAxis::Horizontal => egui::CursorIcon::ResizeHorizontal,
                    SplitAxis::Vertical => egui::CursorIcon::ResizeVertical,
                });
            }
        }
        ResizeResponse { response, delta }
    }
}

/// A pointer drag focuses its handle for the gesture only and gives the focus up on release, so
/// the app's shortcuts work again. A handle focused from the keyboard before the drag keeps its
/// focus.
fn drag_focus(ui: &Ui, response: &Response) {
    let key = response.id.with("pointer-focus");
    if response.drag_started() {
        if !response.has_focus() {
            ui.ctx().data_mut(|d| d.insert_temp(key, ()));
        }
        response.request_focus();
    }
    if response.drag_stopped() && ui.ctx().data_mut(|d| d.remove_temp::<()>(key)).is_some() {
        response.surrender_focus();
    }
}
