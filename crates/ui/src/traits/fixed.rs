use gpui::DefiniteLength;

/// A trait for elements that can have a fixed with. Enables the use of the `width` and `full_width` methods.
pub trait FixedWidth {
    /// Sets the width of the element.
    fn width(self, width: impl Into<DefiniteLength>) -> Self;

    /// Sets the element's width to the full width of its container.
    fn full_width(self) -> Self;
}

/// A trait for elements that can have a fixed height. Enables the use of the `height` and `full_height` methods.
pub trait FixedHeight {
    /// Sets the height of the element.
    fn height(self, height: impl Into<DefiniteLength>) -> Self;

    /// Sets the element's height to the full height of its container, i.e. the space
    /// left over after the container's padding and borders.
    fn full_height(self) -> Self;
}
