//! Public toast notification primitives for extensions and host UIs.
//!
//! Terminal rendering lives in `chatui`; these types define the stable position
//! vocabulary extensions can target, including corners/diagonals and persistent
//! overlay-style widgets.

/// Horizontal anchor for toast placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastX {
    Left,
    Center,
    Right,
}

/// Vertical anchor for toast placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastY {
    Top,
    Middle,
    Bottom,
}

/// Screen position for a toast/overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastPosition {
    pub x: ToastX,
    pub y: ToastY,
}

impl ToastPosition {
    pub const TOP_LEFT: Self = Self { x: ToastX::Left, y: ToastY::Top };
    pub const TOP_CENTER: Self = Self { x: ToastX::Center, y: ToastY::Top };
    pub const TOP_RIGHT: Self = Self { x: ToastX::Right, y: ToastY::Top };
    pub const MIDDLE_LEFT: Self = Self { x: ToastX::Left, y: ToastY::Middle };
    pub const CENTER: Self = Self { x: ToastX::Center, y: ToastY::Middle };
    pub const MIDDLE_RIGHT: Self = Self { x: ToastX::Right, y: ToastY::Middle };
    pub const BOTTOM_LEFT: Self = Self { x: ToastX::Left, y: ToastY::Bottom };
    pub const BOTTOM_CENTER: Self = Self { x: ToastX::Center, y: ToastY::Bottom };
    pub const BOTTOM_RIGHT: Self = Self { x: ToastX::Right, y: ToastY::Bottom };
}

impl Default for ToastPosition {
    fn default() -> Self {
        Self::TOP_CENTER
    }
}
