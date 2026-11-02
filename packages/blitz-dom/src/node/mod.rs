#![allow(clippy::module_inception)]

mod attributes;
#[cfg(feature = "custom-widget")]
mod custom_widget;
mod element;
mod image_orientation;
mod node;
pub(crate) mod resizer;
pub(crate) mod scrollbar;
mod stylo_data;
#[cfg(feature = "svg")]
mod svg;
mod text;
mod writing_mode;

pub use attributes::{Attribute, Attributes};
#[cfg(feature = "custom-widget")]
pub use custom_widget::{
    ComputedStyles, CustomWidgetData, CustomWidgetStatus, ProxyRenderContext, Widget,
};
pub use element::{
    CanvasData, DocumentData, ElementData, ImageData, ImageResourceData, ListItemLayout,
    ListItemLayoutPosition, Marker, MulticolInfo, RasterImageData, SpecialElementData,
    SpecialElementType, Status,
};
pub use node::*;
pub use resizer::{ResizeAxes, ResizedSize};
pub use scrollbar::{ScrollbarColor, ScrollbarRef, ScrollbarWidth};
#[cfg(feature = "svg")]
pub use svg::{SvgImageData, SvgIntrinsicDimensions};
pub use text::{GeneratedTextInputEvent, LineHyphen, TextBrush, TextInputData, TextLayout};
pub(crate) use writing_mode::combines_text;
pub use writing_mode::{LayoutFlow, is_upright};
